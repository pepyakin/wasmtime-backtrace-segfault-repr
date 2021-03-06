use anyhow::anyhow;
use parity_scale_codec::Encode;
use sp_allocator::FreeingBumpHeapAllocator;
use sp_wasm_interface::Pointer;
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use wasmtime::*;

fn default_val(val_ty: &ValType) -> Val {
    match *val_ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        _ => todo!(),
    }
}

fn unpack_ptr_and_len(val: u64) -> (u32, u32) {
    let ptr = (val & (!0u32 as u64)) as u32;
    let len = (val >> 32) as u32;

    (ptr, len)
}

fn read_string(memory: &[u8], ptr: u32, len: u32) -> String {
    let ptr = ptr as usize;
    let len = len as usize;
    String::from_utf8(memory[ptr..(ptr + len)].to_vec()).unwrap()
}

#[derive(Clone)]
struct MemoryHolder {
    inner: Rc<RefCell<Option<Memory>>>, // gross
}

impl MemoryHolder {
    fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(None)),
        }
    }

    fn set(&self, memory: Memory) {
        *self.inner.borrow_mut() = Some(memory);
    }

    fn with<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&Memory) -> R,
    {
        let guard = self.inner.borrow();
        f(&*guard.as_ref().unwrap())
    }
}

struct DummyCallable {
    name: String,
    func_ty: FuncType,
    allocator: Rc<RefCell<FreeingBumpHeapAllocator>>,
    memory: MemoryHolder,
}

impl DummyCallable {
    fn handle_call(&self, params: &[Val], results: &mut [Val]) -> Result<(), Trap> {
        log::debug!(target: "host-call", " {}, params = {:?}", self.name, params);
        results
            .iter_mut()
            .enumerate()
            .for_each(|(idx, result)| *result = default_val(&self.func_ty.params()[idx]));
        match &*self.name {
            "ext_allocator_malloc_version_1" => {
                let size = params[0].unwrap_i32() as u32;
                let ptr = self.memory.with(|memory| {
                    self.allocator
                        .borrow_mut()
                        .allocate(unsafe { memory.data_unchecked_mut() }, size)
                        .map_err(|_| Trap::new("can't allocate"))
                })?;
                results[0] = Val::I32(usize::from(ptr) as i32);
            }
            "ext_allocator_free_version_1" => {
                let ptr = params[0].unwrap_i32() as u32;
                self.memory.with(|memory| {
                    self.allocator
                        .borrow_mut()
                        .deallocate(unsafe { memory.data_unchecked_mut() }, Pointer::new(ptr))
                        .map_err(|_| Trap::new("can't deallocate"))
                })?;
            }
            "ext_logging_log_version_1" => {
                let (target_ptr, target_len) = unpack_ptr_and_len(params[1].unwrap_i64() as u64);
                let (msg_ptr, msg_len) = unpack_ptr_and_len(params[2].unwrap_i64() as u64);
                self.memory.with(|memory| unsafe {
                    let target = read_string(memory.data_unchecked_mut(), target_ptr, target_len);
                    let msg = read_string(memory.data_unchecked_mut(), msg_ptr, msg_len);
                    println!("{}: {}", target, msg);
                });
            }
            _ => {}
        }
        Ok(())
    }
}

impl Callable for DummyCallable {
    fn call(&self, params: &[Val], results: &mut [Val]) -> Result<(), Trap> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::handle_call(self, params, results)
        }))
        .map_err(|_| Trap::new("trap"))
        .and_then(|i| i)
    }
}

fn perform_call(method_name: &str, input_data: &[u8]) -> anyhow::Result<()> {
    let code = fs::read("sc_runtime_test.wasm")?;

    let config = Config::new();
    let engine = Engine::new(&config);

    let store = Store::new(&engine);
    let module = Module::new(&store, &code)?;

    let heap_base = 1055861;
    let allocator = Rc::new(RefCell::new(FreeingBumpHeapAllocator::new(heap_base)));

    let memory = MemoryHolder::new();

    let mut externs = vec![];
    for import in module.imports() {
        match *import.ty() {
            ExternType::Func(ref func_ty) => {
                let callable = DummyCallable {
                    name: import.name().to_string(),
                    func_ty: func_ty.clone(),
                    allocator: allocator.clone(),
                    memory: memory.clone(),
                };
                externs.push(Extern::Func(Func::new(
                    &store,
                    func_ty.clone(),
                    Rc::new(callable),
                )));
            }
            _ => return Err(anyhow!("can't provide non function import")),
        }
    }

    let instance = Instance::new(&module, &externs)?;
    memory.set(
        instance
            .get_export("memory")
            .ok_or_else(|| anyhow!("`memory` should be exported"))?
            .memory()
            .ok_or_else(|| anyhow!("`memory` should be of memory kind"))?
            .clone(),
    );

    let (ptr, len) = inject_input_data(&mut *allocator.borrow_mut(), &memory, input_data)?;

    let _ret_values = instance
        .get_export(method_name)
        .ok_or_else(|| anyhow!("`{}` is not found", method_name))?
        .func()
        .ok_or_else(|| anyhow!("`{}` is not a function", method_name))?
        .call(&[ptr, len])?;

    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    perform_call("test_conditional_panic", &vec![2].encode())?;
    perform_call("test_panic", &[])?;
    Ok(())
}

fn inject_input_data(
    allocator: &mut FreeingBumpHeapAllocator,
    memory: &MemoryHolder,
    data: &[u8],
) -> anyhow::Result<(Val, Val)> {
    memory.with(|memory| unsafe {
        let ptr = allocator.allocate(memory.data_unchecked_mut(), data.len() as u32)?;
        let ptr = usize::from(ptr);

        let dst = &mut memory.data_unchecked_mut()[ptr..(ptr + data.len())];
        dst.copy_from_slice(data);
        Ok((
            Val::I32(ptr as u32 as i32),
            Val::I32(data.len() as u32 as i32),
        ))
    })
}
