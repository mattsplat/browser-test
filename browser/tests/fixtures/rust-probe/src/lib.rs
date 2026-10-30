//! The Rust pack's test page: `#mode` says what to try, `#out` shows what happened.

use unscripted::{Object, document, fetch, set_timeout};

fn out(text: &str) {
    document().query("#out").unwrap().set_text(text);
}

/// Add a line to `#out` (callbacks come in whatever order the event loop runs them).
fn note(text: &str) {
    let previous = document().query("#out").unwrap().text();
    out(&if previous.is_empty() { text.to_string() } else { format!("{previous}; {text}") });
}

fn start() {
    let mode = document().query("#mode").map(|m| m.text()).unwrap_or_default();
    match mode.as_str() {
        // A member the browser lacks is an error the page can handle; a panic in a handler
        // is reported and stops the page.
        "errors" => {
            let e = document().call("fooBar", &[]).unwrap_err();
            out(&e.0);
            document().query("#b").unwrap().on("click", |_: Object| panic!("rust broke"));
        }
        // Nothing outside the page: no files, sockets, environment, processes or threads.
        "doors" => {
            let mut reachable = Vec::new();
            if std::fs::read_to_string("/etc/passwd").is_ok() {
                reachable.push("files");
            }
            if std::fs::write("/tmp/rust-pack-test", "x").is_ok() {
                reachable.push("write");
            }
            if std::net::TcpStream::connect("93.184.215.14:80").is_ok() {
                reachable.push("sockets");
            }
            if std::env::vars().next().is_some() {
                reachable.push("env");
            }
            if std::process::Command::new("ls").output().is_ok() {
                reachable.push("processes");
            }
            if std::thread::Builder::new().spawn(|| ()).is_ok() {
                reachable.push("threads");
            }
            out(if reachable.is_empty() { "none" } else { "some" });
            if !reachable.is_empty() {
                out(&reachable.join(","));
            }
        }
        // Timers and the network come later, from the event loop.
        "async" => {
            println!("hello from Rust");
            set_timeout(|| note("timer ran"), 1);
            fetch("https://example.com/", None).then(|_| note("fetched?"), |error| note(&format!("fetch failed: {error}")));
        }
        // A runaway page is stopped by the time limit.
        "loop" => {
            let mut n: u64 = 0;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        other => out(&format!("unknown mode {other:?}")),
    }
}

unscripted::main!(start);
