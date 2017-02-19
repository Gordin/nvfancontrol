extern crate nvctrl;
use nvctrl::{NvFanController, NvidiaControl, NVCtrlFanControlState};

#[macro_use]
extern crate log;
use log::{Log, LogRecord, LogLevelFilter, LogMetadata, SetLoggerError};

extern crate getopts;
use getopts::Options;

#[cfg(windows)] extern crate ctrlc;
#[cfg(unix)] extern crate nix;
#[cfg(unix)] use nix::sys::signal;

extern crate time;

#[cfg(unix)] extern crate xdg;
#[cfg(unix)] use xdg::BaseDirectories;

use std::io::{BufReader, BufRead};
use std::fs::File;
use std::env;
use std::thread;
use std::process;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering, ATOMIC_BOOL_INIT};
use std::io::Write;
use std::path::PathBuf;

// http://stackoverflow.com/questions/27588416/how-to-send-output-to-stderr
macro_rules! errln(
    ($($arg:tt)*) => (
        match writeln!(&mut ::std::io::stderr(), $($arg)* ) {
            Ok(_) => {},
            Err(x) => panic!("Unable to write to stderr: {}", x),
        }
    )
);

const CONF_FILE: &'static str = "nvfancontrol.conf";
const MIN_VERSION: f32 = 352.09;
const DEFAULT_CURVE: [(u16, u16); 7] = [(41, 20), (49, 30), (57, 45), (66, 55),
                                        (75, 63), (78, 72), (80, 80)];

static RUNNING: AtomicBool = ATOMIC_BOOL_INIT;

struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &LogMetadata) -> bool {
        metadata.level() <= log::max_log_level()
    }

    fn log(&self, record: &LogRecord) {
        if self.enabled(record.metadata()) {
            errln!("{} - {}", record.level(), record.args());
        }
    }
}

impl Logger {
    pub fn new(level: LogLevelFilter) -> Result<(), SetLoggerError> {
        log::set_logger(|max_level| {
            max_level.set(level);
            Box::new(Logger)
        })
    }
}

struct NVFanManager {
    ctrl: NvidiaControl,
    points: Vec<(u16, u16)>,
    on_time: Option<f64>,
    force: bool
}

impl Drop for NVFanManager {

    fn drop(&mut self) {
        debug!("Resetting fan control");
        self.reset_fan().unwrap();
    }

}

impl NVFanManager {
    fn new(
            points: Vec<(u16, u16)>, force: bool, limits: Option<(u16, u16)>
        ) -> Result<NVFanManager, String> {

        let ctrl = try!(NvidiaControl::new(limits));
        let version: f32 = match ctrl.get_version() {
            Ok(v) => {
                v.parse::<f32>().unwrap()
            }
            Err(e) => {
                return Err(format!("Could not get driver version: {}", e))
            }
        };

        if version < MIN_VERSION {
            let err = format!("Unsupported driver version; need >= {:.2}",
                              MIN_VERSION);
            return Err(err);
        }

        debug!("Curve points: {:?}", points);

        let ret = NVFanManager {
            ctrl: ctrl,
            points: points,
            on_time: None,
            force: force
        };

        Ok(ret)
    }

    fn set_fan(&self, speed: i32) -> Result<(), String> {
        try!(self.ctrl.set_ctrl_type(NVCtrlFanControlState::Manual));
        try!(self.ctrl.set_fanspeed(speed));
        Ok(())
    }

    fn reset_fan(&self) -> Result<(), String> {
        try!(self.ctrl.set_ctrl_type(NVCtrlFanControlState::Auto));
        Ok(())
    }

    fn update(&mut self) -> Result<(), String> {

        let temp = try!(self.ctrl.get_temp()) as u16;
        let ctrl_status = try!(self.ctrl.get_ctrl_status());
        let rpm = try!(self.ctrl.get_fanspeed_rpm());

        let utilization = try!(self.ctrl.get_utilization());
        let gutil = utilization.get("graphics");

        let pfirst = self.points.first().unwrap();
        let plast = self.points.last().unwrap();

        if rpm > 0 && !self.force {
            if let NVCtrlFanControlState::Auto = ctrl_status {
                debug!("Fan is enabled on auto control; doing nothing");
                return Ok(());
            };
        }

        if temp < pfirst.0 && self.on_time.is_some() {
            let now = time::precise_time_s();
            let dif = now - self.on_time.unwrap();

            debug!("{} seconds elapsed since fan was last on", dif as u64);

            // if utilization can't be retrieved the utilization leg is
            // always false and ignored
            if dif < 240.0 || gutil.unwrap_or(&-1) > &25 {
                if let Err(e) = self.set_fan(pfirst.1 as i32) { return Err(e); }
            } else {
                debug!("Grace period expired; turning fan off");
                self.on_time = None;
            }
            return Ok(());
        }

        if temp > plast.0 {
            debug!("Temperature outside curve; setting to max");

            if let Err(e) = self.set_fan(plast.1 as i32) { return Err(e); }

            return Ok(());
        }

        for i in 0..(self.points.len()-1) {
            let p1 = self.points[i];
            let p2 = self.points[i+1];

            if temp >= p1.0 && temp < p2.0 {
                let dx = p2.0 - p1.0;
                let dy = p2.1 - p1.1;

                let slope = (dy as f32) / (dx as f32);

                let y = (p1.1 as f32) + (((temp - p1.0) as f32) * slope);

                self.on_time = Some(time::precise_time_s());
                if let Err(e) = self.set_fan(y as i32) { return Err(e); }

                return Ok(());
            }
        }

        // If no point is found then fan should be off
        self.on_time = None;
        if let Err(e) = self.reset_fan() { return Err(e); }

        Ok(())

    }
}

#[cfg(unix)]
extern fn sigint(_: i32) {
    debug!("Interrupt signal");
    RUNNING.store(false, Ordering::Relaxed);
}

#[cfg(windows)]
fn sigint() {
    debug!("Interrupt signal");
    RUNNING.store(false, Ordering::Relaxed);
}

#[cfg(unix)]
fn register_signal_handlers() -> Result<(), String> {
    let sigaction = signal::SigAction::new(signal::SigHandler::Handler(sigint),
                                           signal::SaFlags::empty(),
                                           signal::SigSet::empty());
    for &sig in &[signal::SIGINT, signal::SIGTERM, signal::SIGQUIT] {
        match unsafe { signal::sigaction(sig, &sigaction) } {
            Ok(_) => {} ,
            Err(err) => {
                return Err(format!("Could not register SIG #{:?} handler: {:?}",
                                   sig ,err));
            }
        };
    }
    Ok(())
}

#[cfg(windows)]
fn register_signal_handlers() -> Result<(), String> {
    match ctrlc::set_handler(sigint) {
        Ok(_) => { Ok(()) } ,
        Err(err) => {
            Err(format!("Could not register signal handler: {:?}", err))
        }
    }
}

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} [options]", program);
    println!("{}", opts.usage(&brief));
}

fn make_limits(res: String) -> Result<Option<(u16,u16)>, String> {
    let parts: Vec<&str> = res.split(',').collect();
    if parts.len() == 1 {
        if parts[0] != "0" {
            Err(format!("Invalid option for \"-l\": {}", parts[0]))
        } else {
            Ok(None)
        }
    } else if parts.len() == 2 {
        let lower = match parts[0].parse::<u16>() {
            Ok(num) => num,
            Err(e) => {
                return Err(format!("Could not parse {} as lower limit: {}", parts[0], e));
            }
        };
        let upper = match parts[1].parse::<u16>() {
            Ok(num) => num,
            Err(e) => {
                return Err(format!("Could not parse {} as upper limit: {}", parts[1], e));
            }
        };

        if upper < lower {
            return Err(format!("Lower limit {} is greater than the upper {}", lower, upper));
        }

        if upper > 100 {
            debug!("Upper limit {} is > 100; clipping to 100", upper);
            Ok(Some((lower, 100)))
        } else {
            Ok(Some((lower, upper)))
        }
    } else {
        Err(format!("Invalid argument for \"-l\": {:?}", parts))
    }
}

#[cfg(unix)]
fn find_config_file() -> Option<PathBuf> {
    match BaseDirectories::new() {
        Ok(x) => {
            x.find_config_file(CONF_FILE)
        },
        Err(e) => {
            warn!("Could not find xdg conformant dirs: {}", e);
            None
        }
    }
}

#[cfg(windows)]
fn find_config_file() -> Option<PathBuf> {
    match env::home_dir() {
        Some(path) => {
            let mut conf_path = PathBuf::from(path.to_str().unwrap());
            conf_path.push(CONF_FILE);
            Some(conf_path)
        },
        None => {
            warn!("Could not find home directory; no config file available");
            None
        }
    }
}

fn curve_from_conf(path: PathBuf) -> Result<Vec<(u16,u16)>, String> {

    let mut curve: Vec<(u16, u16)>;

    match File::open(path.to_str().unwrap()) {
        Ok(file) => {
            curve = Vec::new();

            for raw_line in BufReader::new(file).lines() {
                let line = raw_line.unwrap();
                let trimmed = line.trim();
                if trimmed.starts_with('#') {
                    continue;
                }

                let parts = trimmed.split_whitespace()
                                   .collect::<Vec<&str>>();

                if parts.len() < 2 {
                    warn!("Invalid line \"{}\", ignoring", line);
                    continue
                }

                let x = match parts[0].parse::<u16>() {
                    Ok(val) => val,
                    Err(e) => {
                        warn!("Could not parse value {}: {}, ignoring",
                               parts[0], e);
                        continue;
                    }
                };

                let y = match parts[1].parse::<u16>() {
                    Ok(val) => val,
                    Err(e) => {
                        warn!("Could not parse value {}: {}, ignoring",
                               parts[1], e);
                        continue;
                    }
                };

                curve.push((x, y));
            }
            if curve.len() < 2 {
                Err(String::from("At least two points are required for \
                                 the curve"))
            } else {
                Ok(curve)
            }
        },
        Err(e) => Err(format!("Could not read configuration file {:?}: {}",
                      path, e))
    }

}

pub fn main() {

    let args: Vec<String> = env::args().collect();
    let mut opts = Options::new();

    opts.optflag("d", "debug", "Enable debug messages");
    opts.optopt("l", "limits",
        "Comma separated lower and upper limits, use 0 to disable,
        default: 20,80", "LOWER,UPPER");
    opts.optflag("f", "force", "Always use the custom curve even if the fan is
                 already spinning in auto mode");
    opts.optflag("m", "monitor-only", "Do not update the fan speed and control
                 mode; just log temperatures and fan speeds");
    opts.optflag("h", "help", "Print this help message");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(e) => panic!("Could not parse command line: {:?}", e)
    };

    if matches.opt_present("h") {
        print_usage(&args[0].clone(), opts);
        return;
    }

    let log_level = if matches.opt_present("d") {
        LogLevelFilter::Debug
    } else {
        LogLevelFilter::Info
    };

    match Logger::new(log_level) {
        Ok(v) => v,
        Err(err) => panic!("Could not start logger: {:?}", err)
    };

    let force_update = matches.opt_present("f");

    let limits: Option<(u16, u16)>;
    if matches.opt_present("l") {
        match matches.opt_str("l") {
            Some(res) => {
                match make_limits(res) {
                    Ok(lims) => { limits = lims },
                    Err(e) => {
                        error!("{}", e);
                        process::exit(1);
                    }
                }
            },
            None => {
                error!("Option \"-l\" present but no argument provided");
                process::exit(1);
            }
        }
    } else {
        // Default limits
        limits = Some((20, 80));
    }

    match register_signal_handlers() {
        Ok(_) => {},
        Err(e) => {
            error!("{}", e);
            process::exit(1);
        }
    }

    let curve: Vec<(u16, u16)> = match find_config_file() {
        Some(path) => {
            match curve_from_conf(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("{}; using default curve", e);
                    DEFAULT_CURVE.to_vec()
                }
            }
        },
        None => {
            warn!("No config file found; using default curve");
            DEFAULT_CURVE.to_vec()
        }
    };

    let mut mgr = match NVFanManager::new(curve, force_update, limits) {
        Ok(m) => m,
        Err(s) => {
            error!("{}", s);
            process::exit(1);
        }
    };

    info!("NVIDIA driver version: {:.2}",
             mgr.ctrl.get_version().unwrap().parse::<f32>().unwrap());
    info!("NVIDIA graphics adapter: {}", mgr.ctrl.get_adapter().unwrap());

    let timeout = Duration::new(2, 0);
    RUNNING.store(true, Ordering::Relaxed);

    let monitor_only = matches.opt_present("m");
    if monitor_only {
        info!("Option \"-m\" is present; curve will have no actual effect");
    }

    // Main loop
    loop {
        if !RUNNING.load(Ordering::Relaxed) {
            debug!("Exiting");
            break;
        }

        if !monitor_only {
            if let Err(e) = mgr.update() {
                error!("Could not update fan speed: {}", e)
            };
        }

        let graphics_util = match mgr.ctrl.get_utilization().unwrap().get("graphics") {
            Some(v) => *v,
            None => -1
        };

        debug!("Temp: {}; Speed: {} RPM ({}%); Load: {}%; Mode: {}",
            mgr.ctrl.get_temp().unwrap(), mgr.ctrl.get_fanspeed_rpm().unwrap(),
            mgr.ctrl.get_fanspeed().unwrap(), graphics_util,
            match mgr.ctrl.get_ctrl_status() {
                Ok(NVCtrlFanControlState::Auto) => "Auto",
                Ok(NVCtrlFanControlState::Manual) => "Manual",
                Err(_) => "ERR"});

        thread::sleep(timeout);
    }

}
