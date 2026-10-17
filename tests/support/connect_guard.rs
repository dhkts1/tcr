//! Instrument built CLI tests without ever contacting the live proxy or a real upstream.
use std::path::Path;
use std::process::Command;

pub(crate) struct ConnectGuard {
    directory: tempfile::TempDir,
}

impl ConnectGuard {
    pub(crate) fn attach(command: &mut Command, home: &Path) -> Self {
        let directory = tempfile::tempdir_in(home).expect("private network guard directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/connect_guard.c");
        let library = directory.path().join("guard.so");
        let compile = |output: &Path, arguments: &[&str]| {
            let output = Command::new("cc")
                .args(arguments)
                .arg(&source)
                .arg("-o")
                .arg(output)
                .env("HOME", home)
                .env("CODEX_HOME", home.join(".codex"))
                .output()
                .expect("compile test network guard");
            assert!(
                output.status.success(),
                "network guard compiler failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        #[cfg(target_os = "macos")]
        compile(&library, &["-dynamiclib"]);
        #[cfg(not(target_os = "macos"))]
        compile(&library, &["-shared", "-fPIC", "-ldl"]);
        let guarded = |command: &mut Command, port: u16| {
            #[cfg(target_os = "macos")]
            command.env("DYLD_INSERT_LIBRARIES", &library);
            #[cfg(not(target_os = "macos"))]
            command.env("LD_PRELOAD", &library);
            command
                .env("TCR_TEST_DENIED_PORT", port.to_string())
                .env("TCR_TEST_CONNECT_LOG", directory.path().join("attempts"));
            for variable in [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                command.env_remove(variable);
            }
        };
        // Positive control uses an OWNED listener, never the live port. A missing or
        // broken interposer fails here before any first-run status could see port 3456.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 3456);
        let config = directory.path().join("control.json");
        std::fs::write(
            &config,
            serde_json::json!({"proxy": {"port": port}, "accounts": []}).to_string(),
        )
        .unwrap();
        let mut proof = crate::tcr_env::tcr(home);
        proof
            .args(["status", "--json", "--config"])
            .arg(&config)
            .current_dir(directory.path())
            .env("CODEX_HOME", directory.path().join("codex"))
            .env("TCR_CODEX_AUTH", directory.path().join("no-codex.json"));
        guarded(&mut proof, port);
        assert!(
            proof
                .output()
                .expect("network guard positive control")
                .status
                .success(),
            "the guard must reject an owned listener"
        );
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            std::fs::read(directory.path().join("attempts")).unwrap(),
            b"connect\n"
        );
        std::fs::write(directory.path().join("attempts"), []).unwrap();
        guarded(command, 3456);
        Self { directory }
    }

    pub(crate) fn assert_no_connections(&self) {
        assert!(
            std::fs::read(self.directory.path().join("attempts"))
                .unwrap()
                .is_empty(),
            "fixture attempted the live proxy or a non-loopback upstream; the test guard blocked it before connecting"
        );
    }
}
