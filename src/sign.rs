use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use wasmi::{Caller, Engine, ExternRef, Linker, Memory, Module, Store, Table, Val};

const WASM: &[u8] = include_bytes!("../assets/edge_core.wasm");

pub struct Signer {
    engine: Engine,
    module: Module,
}
#[derive(Default)]
struct Host {
    memory: Option<Memory>,
    table: Option<Table>,
}
impl Signer {
    pub fn new() -> Result<Self> {
        ensure!(
            crate::storage::hash(WASM)
                == "7b9d7936b75afc21d0cca11a3eddddea09ecdd782c6064dd1f65d6e674313bfb",
            "内嵌 WASM 校验失败"
        );
        let engine = Engine::default();
        let module = Module::new(&engine, WASM)?;
        Ok(Self { engine, module })
    }
    pub fn sign(
        &self,
        secret: &str,
        method: &str,
        path: &str,
        timestamp: &str,
        nonce: &str,
        body: &str,
    ) -> Result<String> {
        let mut store = Store::new(&self.engine, Host::default());
        let mut linker = Linker::new(&self.engine);
        // These references are only used to carry wasm-bindgen errors, never key material.
        linker.func_wrap(
            "wbg",
            "__wbindgen_cast_2241b6af4c4b2941",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| -> ExternRef {
                let memory = caller.data().memory.unwrap();
                let mut bytes = vec![0; len.max(0) as usize];
                if memory.read(&caller, ptr as usize, &mut bytes).is_err() {
                    return ExternRef::null();
                }
                ExternRef::new(&mut caller, String::from_utf8_lossy(&bytes).into_owned())
            },
        )?;
        linker.func_wrap(
            "wbg",
            "__wbindgen_init_externref_table",
            |mut caller: Caller<'_, Host>| {
                let table = caller.data().table.unwrap();
                let null = Val::ExternRef(ExternRef::null());
                let offset = table.grow(&mut caller, 4, null.clone()).unwrap();
                table.set(&mut caller, 0, null.clone()).unwrap();
                table.set(&mut caller, offset, null.clone()).unwrap();
                table.set(&mut caller, offset + 1, null).unwrap();
                let yes = ExternRef::new(&mut caller, true);
                let no = ExternRef::new(&mut caller, false);
                table
                    .set(&mut caller, offset + 2, Val::ExternRef(yes))
                    .unwrap();
                table
                    .set(&mut caller, offset + 3, Val::ExternRef(no))
                    .unwrap();
            },
        )?;
        let instance = linker
            .instantiate(&mut store, &self.module)?
            .start(&mut store)?;
        store.data_mut().memory = instance.get_memory(&store, "memory");
        store.data_mut().table = instance.get_table(&store, "__wbindgen_externrefs");
        instance
            .get_typed_func::<(), ()>(&store, "__wbindgen_start")?
            .call(&mut store, ())?;
        let memory = instance
            .get_memory(&store, "memory")
            .context("签名模块缺少 memory")?;
        let malloc = instance.get_typed_func::<(i32, i32), i32>(&store, "__wbindgen_malloc")?;
        let mut allocate = |bytes: &[u8]| -> Result<(i32, i32)> {
            let ptr = malloc.call(&mut store, (bytes.len() as i32, 1))?;
            memory.write(&mut store, ptr as usize, bytes)?;
            Ok((ptr, bytes.len() as i32))
        };
        let key = URL_SAFE_NO_PAD.decode(
            secret
                .trim_end_matches('=')
                .replace('+', "-")
                .replace('/', "_"),
        )?;
        let key_args = allocate(&key)?;
        let (m0, m1) = allocate(method.as_bytes())?;
        let (p0, p1) = allocate(path.as_bytes())?;
        let (t0, t1) = allocate(timestamp.as_bytes())?;
        let (n0, n1) = allocate(nonce.as_bytes())?;
        let (b0, b1) = allocate(body.as_bytes())?;
        let init = instance
            .get_typed_func::<(i32, i32), (i32, i32, i32)>(&store, "init_session_key")?
            .call(&mut store, key_args)?;
        ensure!(init.2 == 0 && init.0 != 0, "签名密钥被拒绝");
        let result=instance.get_typed_func::<(i32,i32,i32,i32,i32,i32,i32,i32,i32,i32),(i32,i32,i32,i32)>(&store,"generate_signature")?
            .call(&mut store,(m0,m1,p0,p1,t0,t1,n0,n1,b0,b1))?;
        ensure!(result.3 == 0, "WASM 签名失败");
        let mut bytes = vec![0; result.1 as usize];
        memory.read(&store, result.0 as usize, &mut bytes)?;
        String::from_utf8(bytes).context("WASM 签名非 UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_module_rejects_invalid_session_key() {
        let signer = Signer::new().unwrap();
        let secret = URL_SAFE_NO_PAD.encode([42u8; 32]);
        assert!(signer
            .sign(&secret, "GET", "/test", "1792400000", "test-nonce", "")
            .is_err());
    }
}
