use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use lux::codegen::translate::Translator;
use lux::codegen::yggdrasil::{YggdrasilCompiler, YggdrasilModule};
use lux::driver::session::{Session, SessionConfig};
use tempfile::TempDir;
use ygg_bytecode::Module;
use ygg_interp::{SystemApi, Trap, run_function};
use ygg_term::{Heap, Term};

fn compile_fib() -> (Vec<YggdrasilModule>, String) {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fib.lux"))
            .expect("read fib example");
    let mut session = Session::with_config(PathBuf::new(), SessionConfig::trusted());
    let syntax = session.compile_source(&source).expect("type-check fib");
    let translated = Translator::new().translate_function_modules(&syntax);
    let output = YggdrasilCompiler::compile(
        &translated.modules,
        translated.entry_module.as_deref(),
        translated.entry_arity.unwrap_or(0),
    )
    .expect("compile fib to Yggdrasil");
    let entry = output.entry_module.expect("fib has an entry module");
    (output.modules, entry)
}

#[test]
fn fib_modules_are_verified_and_run_in_yggdrasil_interpreter() {
    let (modules, entry_name) = compile_fib();
    assert_eq!(
        modules.len(),
        2,
        "fib and main are separate hot-code modules"
    );
    for compiled in &modules {
        ygg_bytecode::verify::verify(&compiled.module).expect("backend output must verify");
        let decoded = Module::decode(&compiled.encode()).expect("encoded module must decode");
        ygg_bytecode::verify::verify(&decoded).expect("round-tripped module must verify");
    }

    let entry = modules
        .iter()
        .find(|module| module.name == entry_name)
        .expect("entry module is in output")
        .module
        .clone();
    let entry_function = entry
        .functions
        .iter()
        .position(|function| {
            function.arity == 0
                && entry
                    .atoms
                    .get(function.name_atom as usize)
                    .is_some_and(|name| name == "apply")
        })
        .expect("entry apply/0");
    let mut api = TestApi::new(&modules, &entry_name);
    let result = api
        .run_trampoline(entry_name.clone(), entry_function, Vec::new())
        .expect("execute fib");
    assert_eq!(result.as_int(), Some(55));
}

#[test]
fn cli_writes_yggm_bundle_and_manifest() {
    let workspace = TempDir::new().expect("temporary workspace");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fib.lux");
    let result = Command::new(env!("CARGO_BIN_EXE_lux"))
        .env("LUX_HOME", workspace.path())
        .arg("--yggdrasil")
        .arg(source)
        .output()
        .expect("start lux compiler");
    assert!(
        result.status.success(),
        "Yggdrasil CLI compilation failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );

    let manifest_path = workspace.path().join("artifacts/fib.ygg.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read Yggdrasil manifest"))
            .expect("parse Yggdrasil manifest");
    assert_eq!(manifest["format"], "YGGM1");
    assert_eq!(manifest["entry"]["function"], "apply");
    assert_eq!(manifest["entry"]["arity"], 0);
    let entries = manifest["modules"].as_array().expect("module list");
    assert_eq!(entries.len(), 2);
    for entry in entries {
        let file = entry["file"].as_str().expect("module file");
        let bytes = std::fs::read(workspace.path().join("artifacts").join(file))
            .expect("read .yggm module");
        let module = Module::decode(&bytes).expect("decode .yggm module");
        ygg_bytecode::verify::verify(&module).expect("CLI module must verify");
    }
}

enum TailTarget {
    Ext(u32, u32, Vec<Term>),
    Local(u32, Vec<Term>),
}

struct TestApi {
    _heap_words: Vec<u64>,
    heap: Heap,
    mailbox: VecDeque<Term>,
    modules: HashMap<String, Module>,
    global_atoms: Vec<String>,
    module_atom_maps: HashMap<String, Vec<u32>>,
    current_module: String,
    tail_target: Option<TailTarget>,
}

impl TestApi {
    fn new(modules: &[YggdrasilModule], entry: &str) -> Box<Self> {
        let mut global_atoms = Vec::new();
        let mut global_indices = HashMap::new();
        let mut module_atom_maps = HashMap::new();
        let mut module_table = HashMap::new();
        for compiled in modules {
            let atom_map = compiled
                .module
                .atoms
                .iter()
                .map(|atom| intern_atom(atom, &mut global_atoms, &mut global_indices))
                .collect();
            module_atom_maps.insert(compiled.name.clone(), atom_map);
            module_table.insert(compiled.name.clone(), compiled.module.clone());
        }

        let mut heap_words = vec![0u64; 8192];
        let heap = unsafe {
            Heap::new(
                heap_words.as_mut_ptr().cast(),
                heap_words.len() * std::mem::size_of::<u64>(),
            )
        };
        Box::new(Self {
            _heap_words: heap_words,
            heap,
            mailbox: VecDeque::new(),
            modules: module_table,
            global_atoms,
            module_atom_maps,
            current_module: entry.to_owned(),
            tail_target: None,
        })
    }

    /// Run with tail-call trampolining, the engine contract since TAIL_CALL /
    /// TAIL_CALL_EXT: a returned sentinel means "re-dispatch to the stash".
    fn run_trampoline(
        &mut self,
        mut module_name: String,
        mut function: usize,
        mut args: Vec<Term>,
    ) -> Result<Term, Trap> {
        loop {
            let module = self.modules.get(&module_name).cloned().ok_or(Trap::BadCode)?;
            let previous = std::mem::replace(&mut self.current_module, module_name.clone());
            let result = run_function(&module, function, &args, self);
            self.current_module = previous;
            let value = result?;
            if value != ygg_interp::TAIL_SENTINEL {
                return Ok(value);
            }
            match self.tail_target.take().ok_or(Trap::BadCode)? {
                TailTarget::Ext(module_atom, function_atom, target_args) => {
                    module_name = self
                        .global_atoms
                        .get(module_atom as usize)
                        .cloned()
                        .ok_or(Trap::BadCode)?;
                    let function_name = self
                        .global_atoms
                        .get(function_atom as usize)
                        .cloned()
                        .ok_or(Trap::BadCode)?;
                    let target = self.modules.get(&module_name).ok_or(Trap::BadCode)?;
                    function = target
                        .functions
                        .iter()
                        .position(|f| {
                            f.arity as usize == target_args.len()
                                && target
                                    .atoms
                                    .get(f.name_atom as usize)
                                    .is_some_and(|name| name == &function_name)
                        })
                        .ok_or(Trap::BadCode)?;
                    args = target_args;
                }
                TailTarget::Local(index, target_args) => {
                    function = index as usize;
                    args = target_args;
                }
            }
        }
    }
}

fn intern_atom(atom: &str, atoms: &mut Vec<String>, indices: &mut HashMap<String, u32>) -> u32 {
    if let Some(index) = indices.get(atom) {
        return *index;
    }
    let index = atoms.len() as u32;
    atoms.push(atom.to_owned());
    indices.insert(atom.to_owned(), index);
    index
}

impl SystemApi for TestApi {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn self_pid(&self) -> u64 {
        1
    }

    fn send(&mut self, _to: Term, msg: Term) -> Result<(), Trap> {
        self.mailbox.push_back(msg);
        Ok(())
    }

    fn recv(&mut self) -> Term {
        self.mailbox.pop_front().unwrap_or(Term::NIL)
    }

    fn spawn(&mut self, _fn_idx: u32, _arg: Term) -> Result<u64, Trap> {
        Err(Trap::Badarg)
    }

    fn safepoint(&mut self) {}

    fn atom_global(&mut self, local: u32) -> u32 {
        self.module_atom_maps[&self.current_module][local as usize]
    }

    fn print(&mut self, _term: Term) {}

    fn port_open(&mut self, _kind: u8) -> Result<Term, Trap> {
        Err(Trap::Badarg)
    }

    fn port_submit(&mut self, _port: Term, _op: u8, _arg0: Term, _tag: Term) -> Result<(), Trap> {
        Err(Trap::Badarg)
    }

    fn call_ext(
        &mut self,
        module_atom: u32,
        function_atom: u32,
        arguments: &[Term],
    ) -> Result<Term, Trap> {
        let module_name = self
            .global_atoms
            .get(module_atom as usize)
            .cloned()
            .ok_or(Trap::BadCode)?;
        let function_name = self
            .global_atoms
            .get(function_atom as usize)
            .cloned()
            .ok_or(Trap::BadCode)?;
        let module = self
            .modules
            .get(&module_name)
            .cloned()
            .ok_or(Trap::BadCode)?;
        let function = module
            .functions
            .iter()
            .position(|function| {
                function.arity as usize == arguments.len()
                    && module
                        .atoms
                        .get(function.name_atom as usize)
                        .is_some_and(|name| name == &function_name)
            })
            .ok_or(Trap::BadCode)?;

        let _ = module;
        self.run_trampoline(module_name, function, arguments.to_vec())
    }

    fn tail_call(&mut self, module_atom: u32, function_atom: u32, args: &[Term]) {
        self.tail_target = Some(TailTarget::Ext(module_atom, function_atom, args.to_vec()));
    }

    fn tail_call_local(&mut self, function_index: u32, args: &[Term]) {
        self.tail_target = Some(TailTarget::Local(function_index, args.to_vec()));
    }

    fn buf_to_bin(&mut self, _id: i64) -> Result<Term, Trap> {
        Err(Trap::Badarg) // no kernel packet buffers in this harness
    }

    fn bin_to_buf(&mut self, _bin: Term) -> Result<Term, Trap> {
        Err(Trap::Badarg)
    }
}
