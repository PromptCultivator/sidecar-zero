//! scz-kill-agent: the kill agent of bench §5. Recovery is claimed in milliseconds, so the moment
//! of a kill must be known to better than that: a kill through the Kubernetes API lands tens of
//! milliseconds after it is asked for. This program runs where the process table is (the node,
//! or the container the model runs in), reads `CLOCK_MONOTONIC`, and calls `kill` itself.
//!
//!   scz-kill-agent [--sig=KILL|STOP|CONT|TERM] PID...
//!
//! Prints one line per pid: `KILL <monotonic ns> <pid> <signal> ok`, or the errno. Static, so it
//! can be copied into a Kind node or a server's container.

fn main() {
    let mut sig = libc::SIGKILL;
    let mut name = "KILL".to_string();
    let mut pids = vec![];
    for a in std::env::args().skip(1) {
        if let Some(s) = a.strip_prefix("--sig=") {
            sig = match s {
                "KILL" => libc::SIGKILL,
                "STOP" => libc::SIGSTOP,
                "CONT" => libc::SIGCONT,
                "TERM" => libc::SIGTERM,
                _ => {
                    eprintln!("scz-kill-agent: unknown signal {s}");
                    std::process::exit(2);
                }
            };
            name = s.to_string();
        } else if let Ok(p) = a.parse::<i32>() {
            pids.push(p);
        } else {
            eprintln!("usage: scz-kill-agent [--sig=KILL|STOP|CONT|TERM] PID...");
            std::process::exit(2);
        }
    }
    let mut failed = false;
    for p in pids {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime writes a timespec we own; kill has no memory preconditions.
        let r = unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
            libc::kill(p, sig)
        };
        let t = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;
        if r == 0 {
            println!("KILL {t} {p} {name} ok");
        } else {
            println!("KILL {t} {p} {name} {}", std::io::Error::last_os_error());
            failed = true;
        }
    }
    std::process::exit(i32::from(failed));
}
