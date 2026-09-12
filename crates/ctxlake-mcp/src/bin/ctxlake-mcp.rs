//! The real `ctxlake-mcp` process entry point: `ctxlake_mcp::run_stdio()` on real
//! stdin/stdout, nothing else. Kept this thin on purpose — every behavior worth
//! testing lives in the library (`lib.rs`, `protocol.rs`, ...), which can be
//! driven by an in-memory buffer; this file exists only so a test can also spawn a
//! genuine child process and inspect its *actual* stdout, the one thing an
//! in-process test can never observe (see `tests/stdio_subprocess.rs`).

fn main() {
    ctxlake_mcp::run_stdio();
}
