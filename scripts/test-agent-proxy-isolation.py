#!/usr/bin/env python3
"""Exercise real clients using synthetic logins, local model SSE and HTTPS.

Pass explicit client binaries (not shell aliases) and --tcr to test a build.
The default assertion requires shell HTTPS to bypass tcr; --expect-proxy-tools
is the negative control for an unfixed build. Scratch homes/logs are retained.
This does not test authenticated Remote Control or alter an existing server.
"""

import argparse
import gzip
import http.server
import json
import re
import select
import shlex
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

PROXIES = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
]


def event(kind, **data):
    return f"event: {kind}\ndata: {json.dumps(dict(type=kind, **data))}\n\n"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("client", choices=["claude", "codex"])
    p.add_argument(
        "mode", choices=["baseline", "prefix", "envfile", "both", "filter", "set"]
    )
    p.add_argument("--poison-shell", action="store_true")
    p.add_argument("--nested", action="store_true")
    p.add_argument("--tcr", type=Path)
    p.add_argument("--claude-bin", type=Path, required=True)
    p.add_argument("--codex-bin", type=Path, required=True)
    p.add_argument("--expect-proxy-tools", action="store_true")
    p.add_argument("--policy", choices=["legacy", "filters"])
    p.add_argument("--replace-tcr", action="store_true")
    p.add_argument("--github", action="store_true")
    a = p.parse_args()
    home = Path(
        tempfile.mkdtemp(prefix=f"tcr-agent-probe-{a.client}-{a.mode}-", dir="/tmp")
    )
    state = {
        "connects": [],
        "model_requests": 0,
        "direct_requests": 0,
        "tool_results": [],
    }
    conf = home / "cert.cnf"
    conf.write_text(
        "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=tcr synthetic test\n[ext]\nsubjectAltName=DNS:api.anthropic.com,DNS:chatgpt.com,IP:127.0.0.1\nbasicConstraints=critical,CA:TRUE\nkeyUsage=keyCertSign,digitalSignature,keyEncipherment\n"
    )
    cert, key = home / "cert.pem", home / "key.pem"
    subprocess.run(
        [
            "/usr/bin/openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-config",
            str(conf),
            "-keyout",
            str(key),
            "-out",
            str(cert),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    leafkey, csr, leaf = home / "leaf.key", home / "leaf.csr", home / "leaf.pem"
    with conf.open("a") as f:
        f.write(
            "[leaf]\nsubjectAltName=DNS:api.anthropic.com,DNS:chatgpt.com,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
        )
    subprocess.run(
        [
            "/usr/bin/openssl",
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=tcr synthetic leaf",
            "-keyout",
            str(leafkey),
            "-out",
            str(csr),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        [
            "/usr/bin/openssl",
            "x509",
            "-req",
            "-in",
            str(csr),
            "-CA",
            str(cert),
            "-CAkey",
            str(key),
            "-set_serial",
            "2",
            "-days",
            "1",
            "-extfile",
            str(conf),
            "-extensions",
            "leaf",
            "-out",
            str(leaf),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(leaf, leafkey)
    command = ""

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            pass

        def respond(self, body, content_type="application/json", status=200):
            raw = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            self.wfile.flush()

        def do_CONNECT(self):
            state["connects"].append(self.path)
            self.send_response(200)
            self.end_headers()
            self.wfile.flush()
            try:
                self.connection = context.wrap_socket(self.connection, server_side=True)
                self.rfile = self.connection.makefile("rb")
                self.wfile = self.connection.makefile("wb")
                self.close_connection = False
                while not self.close_connection:
                    self.handle_one_request()
            except (OSError, ssl.SSLError) as e:
                state.setdefault("tls_errors", []).append(str(e))
            self.close_connection = True

        def do_GET(self):
            if self.path.startswith("/direct"):
                state["direct_requests"] += 1
                self.respond("direct-ok\n", "text/plain")
            elif self.path.endswith("/responses"):
                self.respond("{}", status=426)
            else:
                self.respond("{}")

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if self.headers.get("Content-Encoding") == "gzip":
                raw = gzip.decompress(raw)
            try:
                body = json.loads(raw)
            except Exception:
                body = {}
            (home / "last-request.json").write_text(json.dumps(body, indent=2))
            if "/messages" in self.path and "count_tokens" not in self.path:
                if self.headers.get("Authorization") == "Bearer fixture-claude":
                    state["pooled_claude"] = state.get("pooled_claude", 0) + 1
                state["model_requests"] += 1
                results = [
                    b
                    for m in body.get("messages", [])
                    for b in m.get("content", [])
                    if isinstance(b, dict) and b.get("type") == "tool_result"
                ]
                state["tool_results"] = results
                tool = not results
                start = {
                    "id": "msg_fixture",
                    "type": "message",
                    "role": "assistant",
                    "model": body.get("model", "claude-sonnet-4-6"),
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                }
                out = event("message_start", message=start)
                if tool:
                    out += event(
                        "content_block_start",
                        index=0,
                        content_block={
                            "type": "tool_use",
                            "id": "toolu_fixture",
                            "name": "Bash",
                            "input": {},
                        },
                    )
                    out += event(
                        "content_block_delta",
                        index=0,
                        delta={
                            "type": "input_json_delta",
                            "partial_json": json.dumps(
                                {
                                    "command": command,
                                    "description": "Probe local test routing",
                                }
                            ),
                        },
                    )
                else:
                    out += event(
                        "content_block_start",
                        index=0,
                        content_block={"type": "text", "text": ""},
                    )
                    out += event(
                        "content_block_delta",
                        index=0,
                        delta={"type": "text_delta", "text": "probe complete"},
                    )
                out += event("content_block_stop", index=0)
                out += event(
                    "message_delta",
                    delta={
                        "stop_reason": "tool_use" if tool else "end_turn",
                        "stop_sequence": None,
                    },
                    usage={"output_tokens": 15},
                )
                out += event("message_stop")
                self.respond(out, "text/event-stream")
            elif self.path.endswith("/responses"):
                if self.headers.get("Authorization") == "Bearer fixture-codex":
                    state["pooled_codex"] = state.get("pooled_codex", 0) + 1
                state["model_requests"] += 1
                state["codex_requests"] = state.get("codex_requests", 0) + 1
                results = [
                    i
                    for i in body.get("input", [])
                    if isinstance(i, dict)
                    and i.get("type")
                    in ["function_call_output", "custom_tool_call_output"]
                ]
                state["tool_results"] = results
                # A tool result may be supplied using previous_response_id.
                tool = state["codex_requests"] == 1 and not a.nested
                item = (
                    {
                        "type": "function_call",
                        "id": "fc_fixture",
                        "call_id": "call_fixture",
                        "name": "exec_command",
                        "arguments": json.dumps(
                            {
                                "cmd": command,
                                "yield_time_ms": 1000,
                                "max_output_tokens": 1000,
                            }
                        ),
                    }
                    if tool
                    else {
                        "type": "message",
                        "id": "msg_fixture",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "probe complete"}],
                    }
                )
                out = event(
                    "response.created",
                    response={
                        "id": f"resp_{state['model_requests']}",
                        "status": "in_progress",
                        "output": [],
                    },
                )
                out += event("response.output_item.added", output_index=0, item=item)
                out += event("response.output_item.done", output_index=0, item=item)
                out += event(
                    "response.completed",
                    response={
                        "id": f"resp_{state['model_requests']}",
                        "status": "completed",
                        "output": [item],
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 10,
                            "total_tokens": 20,
                        },
                    },
                )
                self.respond(out, "text/event-stream")
            else:
                self.respond("{}")

    direct = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    direct.socket = context.wrap_socket(direct.socket, server_side=True)
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    for server in [direct, proxy]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    proxy_url = f"http://127.0.0.1:{proxy.server_port}"
    if a.poison_shell:
        (home / ".bashrc").write_text(
            f"export HTTPS_PROXY={shlex.quote(proxy_url)}\nexport https_proxy={shlex.quote(proxy_url)}\n"
        )
        (home / ".bash_profile").write_text('. "$HOME/.bashrc"\n')
    command = f'/usr/bin/curl --fail --silent --show-error --max-time 10 --cacert {shlex.quote(str(cert))} https://127.0.0.1:{direct.server_port}/direct; /usr/bin/env | /usr/bin/grep -E "^(HTTPS_PROXY|https_proxy|HTTP_PROXY|http_proxy|ALL_PROXY|all_proxy)="; /usr/bin/touch {shlex.quote(str(home / "tool-ran"))}'
    env = {
        "HOME": str(home),
        "PATH": "/usr/bin:/bin:/opt/homebrew/bin",
        "SHELL": "/bin/bash",
        "TERM": "dumb",
        "HTTPS_PROXY": proxy_url,
        "https_proxy": proxy_url,
        "NODE_EXTRA_CA_CERTS": str(cert),
        "CODEX_CA_CERTIFICATE": str(cert),
        "NO_COLOR": "1",
    }
    cleanup = home / "tool-env.sh"
    cleanup.write_text("unset " + " ".join(PROXIES) + "\n")
    prefix = home / "shell-prefix"
    prefix.write_text(
        "#!/bin/sh\nunset " + " ".join(PROXIES) + '\nexec /bin/bash -c "$1"\n'
    )
    prefix.chmod(0o700)
    if a.nested:
        bindir = home / "bin"
        bindir.mkdir()
        nested = bindir / "codex"
        nested.write_text(
            "#!/bin/sh\nexport HTTPS_PROXY="
            + shlex.quote(proxy_url)
            + "\nexport https_proxy="
            + shlex.quote(proxy_url)
            + "\nexec "
            + shlex.quote(str(a.codex_bin))
            + ' "$@"\n'
        )
        nested.chmod(0o700)
        if not a.tcr:
            env["PATH"] = str(bindir) + ":" + env["PATH"]
        env["CODEX_HOME"] = str(home / ".codex")
        Path(env["CODEX_HOME"]).mkdir()
        nested_args = [
            "codex",
            "-c",
            'model_provider="fixture"',
            "-c",
            'model_providers.fixture={name="fixture",base_url="https://chatgpt.com/backend-api/codex",wire_api="responses"}',
            "-c",
            "check_for_update_on_startup=false",
            "-c",
            "analytics.enabled=false",
            "-c",
            "feedback.enabled=false",
            "exec",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--model",
            "fixture",
            "Return probe complete.",
        ]
        command += "; " + shlex.join(nested_args)
    if a.client == "claude":
        env.update(
            {
                "CLAUDE_CONFIG_DIR": str(home / ".claude"),
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_API_KEY": "synthetic-fixture-key",
                "DISABLE_AUTOUPDATER": "1",
                "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_GZIP_REQUEST_BODIES": "0",
            }
        )
        if a.mode in ["prefix", "both"]:
            env["CLAUDE_CODE_SHELL_PREFIX"] = str(prefix)
        if a.mode in ["envfile", "both"]:
            env["CLAUDE_ENV_FILE"] = str(cleanup)
        args = [
            str(a.claude_bin),
            "-p",
            "Run the local fixture tool once.",
            "--permission-mode",
            "bypassPermissions",
            "--model",
            "claude-sonnet-4-6",
            "--max-turns",
            "3",
            "--output-format",
            "json",
            "--debug-file",
            str(home / "debug.log"),
        ]
    else:
        env["CODEX_HOME"] = str(home / ".codex")
        Path(env["CODEX_HOME"]).mkdir()
        args = [
            str(a.codex_bin),
            "-c",
            'model_provider="fixture"',
            "-c",
            'model_providers.fixture={name="fixture",base_url="https://chatgpt.com/backend-api/codex",wire_api="responses"}',
            "-c",
            "check_for_update_on_startup=false",
            "-c",
            "analytics.enabled=false",
            "-c",
            "feedback.enabled=false",
        ]
        if a.mode == "filter":
            for key in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"]:
                args += ["-c", f'shell_environment_policy.filters.{key}="exclude"']
        if a.mode == "set":
            for key in PROXIES:
                args += ["-c", f'shell_environment_policy.set.{key}=""']
        args += [
            "exec",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--model",
            "fixture",
            "Run the local fixture tool once.",
        ]
    if a.policy:
        assert a.client == "codex"
        policy = (
            'exclude = ["UNRELATED_PRIVATE"]'
            if a.policy == "legacy"
            else 'filters = { UNRELATED_PRIVATE = "exclude" }'
        )
        (Path(env["CODEX_HOME"]) / "config.toml").write_text(
            "[shell_environment_policy]\n" + policy + '\nset = { KEEP_ME = "kept" }\n'
        )
        command += '; test "$KEEP_ME" = kept && printf "policy-preserved\\n"'
    if a.replace_tcr:
        assert a.tcr
        curl = f"/usr/bin/curl --fail --silent --show-error --max-time 10 --cacert {shlex.quote(str(cert))} https://127.0.0.1:{direct.server_port}/direct"
        command = (
            f"touch {shlex.quote(str(home / 'replace-now'))}; "
            f"for attempt in {' '.join(str(i) for i in range(100))}; do "
            f"{curl} || exit 1; test -f {shlex.quote(str(home / 'replaced'))} && break; sleep .1; done; "
            f"test -f {shlex.quote(str(home / 'replaced'))} || exit 1; "
        ) + command
    if a.github:
        github = '/usr/bin/curl --fail --silent --show-error --max-time 15 -w "\\ngithub-direct remote=%{remote_ip} seconds=%{time_total}\\n" https://api.github.com/zen || exit 1; '
        command = github + command + "; " + github
    tcr = None
    successors = []
    replacement = None
    if a.tcr:
        assert a.mode == "baseline", (
            "With --tcr, the launcher itself must configure isolation."
        )
        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        config = home / "tcr.json"
        data = {
            "proxy": {"port": 0},
            "upstream": f"http://127.0.0.1:{upstream.server_port}",
            "codexUpstream": f"http://127.0.0.1:{upstream.server_port}",
            "claudeCodeImport": False,
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            "accounts": [
                {
                    "name": "alice@example.com",
                    "accessToken": "fixture-claude",
                    "expiresAt": 4102444800000,
                },
                {
                    "name": "bob@example.com",
                    "provider": "codex",
                    "accountUuid": "11111111-1111-4111-8111-111111111111",
                    "accessToken": "fixture-codex",
                    "expiresAt": 4102444800000,
                },
            ],
        }
        config.write_text(json.dumps(data))
        server_env = {
            "HOME": str(home),
            "PATH": "/usr/bin:/bin",
            "RUST_LOG": "info",
            "TCR_CLAUDE_CODE_CREDENTIALS": str(home / "no-claude-login"),
            "TCR_CODEX_AUTH": str(home / "no-codex-login"),
        }
        log = home / "tcr.log"
        with log.open("wb") as out:
            tcr = subprocess.Popen(
                [
                    str(a.tcr.resolve()),
                    "server",
                    "--headless",
                    "--config",
                    str(config),
                    "--port",
                    "0",
                ],
                cwd=home,
                env=server_env,
                stdout=out,
                stderr=subprocess.STDOUT,
            )
        deadline = time.monotonic() + 25
        while True:
            found = re.search(
                r"listening on http://127\.0\.0\.1:(\d+)", log.read_text()
            )
            if found:
                break
            if tcr.poll() is not None or time.monotonic() > deadline:
                tcr.kill()
                tcr.wait()
                raise RuntimeError("test server did not start: " + log.read_text())
            time.sleep(0.05)
        actual_port = int(found[1])

        class Forward(socketserver.BaseRequestHandler):
            def handle(self):
                with socket.create_connection(
                    ("127.0.0.1", actual_port), timeout=10
                ) as peer:
                    first = self.request.recv(65536)
                    line = first.split(b"\r\n", 1)[0].decode("ascii", errors="replace")
                    if line.startswith("CONNECT "):
                        state["connects"].append(line.split()[1])
                    peer.sendall(first)
                    sockets = [self.request, peer]
                    while True:
                        ready, _, _ = select.select(sockets, [], [], 30)
                        if not ready:
                            return
                        for source in ready:
                            try:
                                chunk = source.recv(65536)
                            except OSError:
                                return
                            if not chunk:
                                return
                            try:
                                (
                                    peer if source is self.request else self.request
                                ).sendall(chunk)
                            except OSError:
                                return

        forward = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Forward)
        forward.daemon_threads = True
        threading.Thread(target=forward.serve_forever, daemon=True).start()
        data["proxy"]["port"] = forward.server_address[1]
        config.write_text(json.dumps(data))
        for name in PROXIES + ["NODE_EXTRA_CA_CERTS", "CODEX_CA_CERTIFICATE"]:
            env.pop(name, None)
        env.update(
            {
                "TCR_CLAUDE_BIN": str(a.claude_bin),
                "TCR_CODEX_BIN": str(a.codex_bin),
                **{k: v for k, v in server_env.items() if k.startswith("TCR_")},
            }
        )
        args = (
            [str(a.tcr.resolve()), "run", "--config", str(config)]
            + (["--codex"] if a.client == "codex" else [])
            + ["--"]
            + args[1:]
        )
        if a.replace_tcr:

            def replace():
                deadline = time.monotonic() + 40
                while not (home / "replace-now").exists():
                    if time.monotonic() > deadline:
                        state["replacement_error"] = "shell never started"
                        return
                    time.sleep(0.02)
                log = home / "successor.log"
                with log.open("wb") as out:
                    child = subprocess.Popen(
                        [
                            str(a.tcr.resolve()),
                            "server",
                            "--headless",
                            "--replace",
                            "--config",
                            str(config),
                            "--port",
                            str(actual_port),
                        ],
                        cwd=home,
                        env=server_env,
                        stdout=out,
                        stderr=subprocess.STDOUT,
                    )
                successors.append(child)
                while (
                    f"listening on http://127.0.0.1:{actual_port}"
                    not in log.read_text()
                ):
                    if child.poll() is not None or time.monotonic() > deadline:
                        state["replacement_error"] = log.read_text()
                        return
                    time.sleep(0.02)
                (home / "replaced").touch()
                state["replaced"] = True

            replacement = threading.Thread(target=replace, daemon=True)
            replacement.start()
    try:
        with (
            (home / "stdout.log").open("wb") as out,
            (home / "stderr.log").open("wb") as err,
        ):
            run = subprocess.run(
                args,
                cwd=home,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=err,
                timeout=50,
            )
            state["exit"] = run.returncode
    except subprocess.TimeoutExpired:
        state["exit"] = "timeout"
    finally:
        if replacement is not None:
            replacement.join(timeout=45)
        if tcr is not None:
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                with opener.open(
                    f"http://127.0.0.1:{actual_port}/_tcr/status", timeout=5
                ) as response:
                    (home / "status.json").write_bytes(response.read())
            finally:
                tcr.kill()
                tcr.wait()
                for child in successors:
                    child.kill()
                    child.wait()
    state["tool_ran"] = (home / "tool-ran").exists()
    state["home"] = str(home)
    state["direct_port"] = direct.server_port
    (home / "result.json").write_text(json.dumps(state, indent=2))
    print(json.dumps(state, indent=2))
    assert state["exit"] == 0 and state["tool_ran"], (
        "the real client must execute its shell tool successfully"
    )
    tool_target = f"127.0.0.1:{direct.server_port}"
    assert (tool_target in state["connects"]) == a.expect_proxy_tools, (
        "shell HTTPS request took the wrong route"
    )
    assert state["direct_requests"] >= 1, "shell HTTPS request must succeed"
    assert state["model_requests"] >= 2, "model calls must still reach the proxy"
    if a.nested:
        assert state.get("codex_requests", 0) > 0, "nested Codex must reach the proxy"
    if a.tcr:
        for provider in ["claude", "codex"] if a.nested else [a.client]:
            assert state.get("pooled_" + provider, 0) > 0, (
                "model calls must use tcr's synthetic account credential"
            )
    if a.policy:
        assert "policy-preserved" in json.dumps(state["tool_results"]), (
            "unrelated shell settings must survive"
        )
    if a.replace_tcr:
        assert state.get("replaced"), state.get("replacement_error")
    if a.github:
        results = json.dumps(state["tool_results"])
        assert results.count("github-direct remote=") == 2, (
            "both real GitHub requests must succeed"
        )
        assert "github-direct remote=127." not in results, (
            "GitHub curl must not connect to tcr"
        )


if __name__ == "__main__":
    main()
