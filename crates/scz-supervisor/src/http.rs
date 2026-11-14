//! The HTTP probes (supervisor §7). The listener caps concurrent connections and drops a
//! request that has not arrived whole within a second, so a slow client cannot hold the loop.

use crate::machine::Core;
use crate::{describe, Epoll, K_HTTP, T_HTTP};
use scz_shm::abi::gate_state as gs;
use scz_shm::word;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsFd;

const MAX_CONNS: usize = 16;

pub struct Http {
    listener: TcpListener,
    conns: HashMap<u64, (TcpStream, Vec<u8>, u64)>,
    next: u64,
}

impl Http {
    pub fn bind(addr: &str, ep: &Epoll) -> io::Result<Http> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        ep.add(listener.as_fd(), T_HTTP)?;
        Ok(Http { listener, conns: HashMap::new(), next: 0 })
    }

    pub fn accept(&mut self, ep: &Epoll) {
        while let Ok((s, _)) = self.listener.accept() {
            if self.conns.len() >= MAX_CONNS || s.set_nonblocking(true).is_err() {
                continue; // dropped: the probe retries
            }
            let id = self.next;
            self.next += 1;
            if ep.add(s.as_fd(), K_HTTP | id).is_ok() {
                self.conns.insert(id, (s, Vec::new(), scz_shm::wait::monotonic_ns()));
            }
        }
    }

    pub fn expire(&mut self, now: u64, ep: &Epoll) {
        let old: Vec<u64> = self.conns.iter().filter(|(_, c)| c.2 + 1_000_000_000 < now).map(|(&k, _)| k).collect();
        for k in old {
            self.close(k, ep);
        }
    }

    fn close(&mut self, id: u64, ep: &Epoll) {
        if let Some((s, _, _)) = self.conns.remove(&id) {
            ep.del(s.as_fd());
        }
    }

    /// Read what has arrived on a connection and, once the request line is whole, answer it
    /// with `respond(path) -> (status, body)`.
    pub fn serve(&mut self, id: u64, ep: &Epoll, respond: impl Fn(&str) -> (u16, String)) {
        let Some((s, buf, _)) = self.conns.get_mut(&id) else { return };
        let mut chunk = [0u8; 512];
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return self.close(id, ep),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        let path = match request_path(buf) {
            Err(()) => return self.close(id, ep),
            Ok(None) => return,
            Ok(Some(p)) => p,
        };
        let (code, body) = respond(&path);
        let reason = match code {
            200 => "OK",
            404 => "Not Found",
            _ => "Service Unavailable",
        };
        let _ = write!(s, "HTTP/1.0 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        self.close(id, ep);
    }
}

/// The path of the request line in `buf`: `Ok(None)` until the line is whole, `Err` for a
/// request that is too long to be a probe. Anything else is answered, if only with a 404.
pub fn request_path(buf: &[u8]) -> Result<Option<String>, ()> {
    if buf.len() > MAX_REQUEST {
        return Err(());
    }
    let Some(end) = buf.windows(2).position(|w| w == b"\r\n") else { return Ok(None) };
    let line = String::from_utf8_lossy(&buf[..end]);
    Ok(Some(line.split(' ').nth(1).unwrap_or("").to_string()))
}

const MAX_REQUEST: usize = 2048;

/// The answers while serving.
pub fn running(path: &str, core: &Core<'_>, since_tick_ns: u64, model_modified: u64, wait_for_model: bool) -> (u16, String) {
    match path {
        "/livez" if since_tick_ns < 1_000_000_000 => (200, "ok".to_string()),
        "/livez" => (503, "the loop has not ticked for a second".to_string()),
        "/readyz" => (200, "ok".to_string()),
        "/startupz" => {
            if wait_for_model && word::gate_state(core.gate()) != gs::HEALTHY {
                (503, format!("gate={}\n", gs::name(word::gate_state(core.gate()))))
            } else {
                (200, "ok".to_string())
            }
        }
        "/modelz" => (if word::gate_state(core.gate()) == gs::HEALTHY { 200 } else { 503 }, describe(core)),
        "/metrics" => (200, metrics(core, model_modified)),
        _ => (404, "not found".to_string()),
    }
}

/// The answers during a degraded start (§3): readiness passes, /startupz waits if asked, /modelz names the cause.
pub fn degraded(path: &str, cause: &str, wait_for_model: bool) -> (u16, String) {
    match path {
        "/livez" | "/readyz" => (200, "ok".to_string()),
        "/startupz" => {
            if wait_for_model {
                (503, format!("degraded: {cause}\n"))
            } else {
                (200, "ok".to_string())
            }
        }
        "/modelz" => (503, format!("{{\"state\":\"DEGRADED\",\"cause\":\"{}\"}}", cause.replace('"', "'"))),
        "/metrics" => (200, format!("scz_degraded 1\n{}", shield_metrics())),
        _ => (404, "not found".to_string()),
    }
}

fn metrics(core: &Core<'_>, model_modified: u64) -> String {
    let s = &core.stats;
    let g = core.gate();
    let mut m = format!(
        "scz_gate_state {}\nscz_gate_generation {}\nscz_worker_spawns_total {}\nscz_worker_deaths_total {}\nscz_standby_promotions_total {}\nscz_worker_restarts_total {}\nscz_reloads_total {}\nscz_worker_condemned_total {}\nscz_worker_crashloop {}\nscz_promotion_seconds {:.6}\nscz_memory_utilization_ratio {:.3}\nscz_shedding {}\nscz_standby_suppressed {}\nscz_session_reset_stuck {}\nscz_degraded 0\n",
        word::gate_state(g),
        word::gate_gen(g),
        s.spawns,
        s.deaths,
        s.promotions,
        s.restarts,
        s.reloads,
        s.condemned,
        u8::from(core.crashloop()),
        s.last_promotion_ns as f64 / 1e9,
        s.util,
        u8::from(s.shedding),
        u8::from(s.standby_suppressed),
        u8::from(s.reset_stuck),
    );
    // What the example alert rules read (kubernetes-deploy §9, deploy/helm-kep753/examples).
    let (rejects, ring_faults) = core.client_counters();
    let host_exits = s.resets.saturating_sub(s.resets_requested);
    m += &format!(
        "scz_model_fit {}\nscz_model_rollbacks_total {}\nscz_models_rejected_total {}\nscz_load_timeouts_total {}\nscz_model_modified_total {model_modified}\n\
         scz_handoffs_total{{result=\"ok\"}} {}\nscz_handoffs_total{{result=\"failed\"}} {}\n\
         scz_session_resets_total{{cause=\"host_exit\"}} {host_exits}\nscz_session_resets_total{{cause=\"requested\"}} {}\n\
         scz_client_rejects_total {rejects}\nscz_client_ring_faults_total {ring_faults}\n",
        u8::from(!s.does_not_fit),
        s.rollbacks,
        s.rejected,
        s.load_timeouts,
        s.handoffs_ok,
        s.handoffs_failed,
        s.resets_requested,
    );
    if let Some(p) = &core.published {
        m += &format!("scz_model_info{{sha256=\"{}\",gen=\"{}\"}} 1\n", crate::machine::hex(&p.sha256), word::gate_gen(g));
    }
    m += &shield_metrics();
    m
}

/// The OOM shield in effect (kubernetes-deploy §5), and the kernel's OOM kills in the
/// container's cgroup, which a shield that works keeps to the worker.
fn shield_metrics() -> String {
    let mode = crate::shield::SETTLED.get().map_or("none", |s| s.0.name());
    let mut m = format!("scz_oom_shield_mode{{mode=\"{mode}\"}} 1\n");
    if let Some((kill, group)) = crate::shield::kernel_oom_events() {
        m += &format!("scz_kernel_oom_events_total{{kind=\"oom_kill\"}} {kill}\nscz_kernel_oom_events_total{{kind=\"oom_group_kill\"}} {group}\n");
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metrics_carry_what_the_example_alerts_read() {
        use scz_shm::layout::{Config as ArenaConfig, Geometry, LocalArena, SlabClass};
        let g = Geometry { rings: vec![8], slab_classes: vec![SlabClass { slot_size: 128, slot_count: 4, req_cap: 64, resp_cap: 64 }], staging_len: 0x1000 };
        let a = LocalArena::new(&g, &ArenaConfig::with_defaults(1, 1)).unwrap();
        let mut core = Core::new(crate::machine::Config::with_defaults(5_000_000, 15_000_000, 100_000_000), a.ctl(), a.data(), &a.dir, 1);
        core.stats.resets = 3;
        core.stats.resets_requested = 1;
        core.stats.handoffs_failed = 2;
        let (code, m) = running("/metrics", &core, 0, 4, false);
        assert_eq!(code, 200);
        for line in [
            "scz_model_fit 1\n",
            "scz_load_timeouts_total 0\n",
            "scz_model_modified_total 4\n",
            "scz_handoffs_total{result=\"failed\"} 2\n",
            "scz_session_resets_total{cause=\"host_exit\"} 2\n",
            "scz_session_resets_total{cause=\"requested\"} 1\n",
            "scz_client_rejects_total 0\n",
            "scz_client_ring_faults_total 0\n",
            "scz_oom_shield_mode{mode=",
        ] {
            assert!(m.contains(line), "missing {line:?} in\n{m}");
        }
        assert!(!m.contains("scz_session_resets_total 3"), "one family, labelled by cause");
    }

    #[test]
    fn startupz_honors_wait_for_model() {
        use scz_shm::layout::{Config as ArenaConfig, Geometry, LocalArena, SlabClass};
        let g = Geometry { rings: vec![8], slab_classes: vec![SlabClass { slot_size: 128, slot_count: 4, req_cap: 64, resp_cap: 64 }], staging_len: 0x1000 };
        let a = LocalArena::new(&g, &ArenaConfig::with_defaults(1, 1)).unwrap();
        let core = Core::new(crate::machine::Config::with_defaults(5_000_000, 15_000_000, 100_000_000), a.ctl(), a.data(), &a.dir, 1);
        // Gate starts in INIT (not HEALTHY).
        assert_eq!(running("/startupz", &core, 0, 0, false).0, 200);
        let (code, body) = running("/startupz", &core, 0, 0, true);
        assert_eq!(code, 503);
        assert!(body.contains("gate="), "{body}");
        assert_eq!(degraded("/startupz", "arena failed", false).0, 200);
        assert_eq!(degraded("/startupz", "arena failed", true).0, 503);
    }

    #[test]
    fn the_request_line_is_found_and_only_that() {
        assert_eq!(request_path(b"GET /livez HTTP/1.1\r\nHost: x\r\n\r\n"), Ok(Some("/livez".to_string())));
        assert_eq!(request_path(b"GET /livez HTTP/1.1"), Ok(None));
        assert_eq!(request_path(b"\r\n"), Ok(Some(String::new())));
        assert_eq!(request_path(&[b'a'; MAX_REQUEST + 1]), Err(()));
        assert_eq!(request_path(b"GET \xff\xfe HTTP/1.1\r\n"), Ok(Some("\u{fffd}\u{fffd}".to_string())));
    }

    #[test]
    fn any_bytes_are_parsed_without_a_panic() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 2100) as usize;
            let alphabet = b"GET /\r\n \xff\x00z";
            let buf: Vec<u8> = (0..n).map(|i| alphabet[((x >> (i % 56)) as usize + i) % alphabet.len()]).collect();
            let r = request_path(&buf);
            if n > MAX_REQUEST {
                assert_eq!(r, Err(()));
            }
        }
    }
}
