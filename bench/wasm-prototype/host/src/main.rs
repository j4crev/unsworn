//! What an in-process WebAssembly handler call costs, against the same
//! code called natively.
use std::time::Instant;
use wasmtime::*;

const HELLO: &str = r#"{"type":"request","id":"r-81","phase":"handle","handler":"src/h.rs#text","route":"/text","method":"GET","path":"/text","headers":{"accept":"*/*","host":"127.0.0.1:8080","user-agent":"wrk"},"deadline_ms":29999}"#;
const SEARCH: &str = r#"{"type":"request","id":"r-81","phase":"handle","handler":"src/h.rs#search","route":"/search","method":"GET","path":"/search","query":{"q":"gramorstatel"},"headers":{"accept":"*/*","host":"127.0.0.1:8080"},"deadline_ms":29999}"#;

struct Guest {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    handle: TypedFunc<(u32, u32), u64>,
}

impl Guest {
    fn new(engine: &Engine, pre: &InstancePre<()>) -> Guest {
        let mut store = Store::new(engine, ());
        let instance = pre.instantiate(&mut store).unwrap();
        Guest {
            memory: instance.get_memory(&mut store, "memory").unwrap(),
            alloc: instance
                .get_typed_func(&mut store, "unsworn_alloc")
                .unwrap(),
            free: instance.get_typed_func(&mut store, "unsworn_free").unwrap(),
            handle: instance
                .get_typed_func(&mut store, "unsworn_handle")
                .unwrap(),
            store,
        }
    }

    fn call(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let ptr = self
            .alloc
            .call(&mut self.store, input.len() as u32)
            .unwrap();
        self.memory
            .write(&mut self.store, ptr as usize, input)
            .unwrap();
        let packed = self
            .handle
            .call(&mut self.store, (ptr, input.len() as u32))
            .unwrap();
        let (optr, olen) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        out.clear();
        out.extend_from_slice(&self.memory.data(&self.store)[optr..optr + olen]);
        self.free
            .call(&mut self.store, (optr as u32, olen as u32))
            .unwrap();
    }
}

fn time(label: &str, n: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..n / 10 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        best = best.min(t.elapsed().as_secs_f64() * 1e6 / n as f64);
    }
    println!("{label:<46} {best:>10.2} µs");
    best
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut config = Config::new();
    config.cranelift_opt_level(OptLevel::Speed);
    let engine = Engine::new(&config).unwrap();
    let t = Instant::now();
    let module = Module::from_file(&engine, &path).unwrap();
    println!(
        "compiling the module: {:.1} ms ({} KB of wasm)",
        t.elapsed().as_secs_f64() * 1e3,
        std::fs::metadata(&path).unwrap().len() / 1024
    );
    let pre = Linker::new(&engine).instantiate_pre(&module).unwrap();
    let mut out = Vec::new();

    let mut guest = Guest::new(&engine, &pre);
    guest.call(SEARCH.as_bytes(), &mut out);
    let wasm_answer = out.clone();
    let same = wasm_answer == guest::handle_message(SEARCH.as_bytes());
    println!("the two give the same answer: {same}");
    assert!(same);
    println!();

    let a = time("hello, native (parse, answer, serialize)", 200_000, || {
        std::hint::black_box(guest::handle_message(HELLO.as_bytes()));
    });
    let b = time("hello, wasm, one instance kept", 200_000, || {
        guest.call(HELLO.as_bytes(), &mut out)
    });
    let c = time("hello, wasm, a new instance each call", 20_000, || {
        let mut g = Guest::new(&engine, &pre);
        g.call(HELLO.as_bytes(), &mut out);
    });
    println!(
        "  the call into wasm adds {:.2} µs; a fresh instance adds {:.2} µs more",
        b - a,
        c - b
    );
    println!();
    let d = time("search, native", 2_000, || {
        std::hint::black_box(guest::handle_message(SEARCH.as_bytes()));
    });
    let e = time("search, wasm, one instance kept", 2_000, || {
        guest.call(SEARCH.as_bytes(), &mut out)
    });
    println!("  wasm takes {:.2} times as long as native", e / d);
}
