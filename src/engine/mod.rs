//! The WebAssembly Execution Engine
//!
//! This module contains the Vector transparent WebAssembly Engine.

// TODO: FreeBSD: https://github.com/bytecodealliance/lucet/pull/419

use crate::{Error, Event, Result};
use lru::LruCache;
use lucet_runtime::c_api::*;
use lucet_runtime::{
    DlModule, Instance, InstanceBuilder, InstanceHandle, Limits, MmapRegion, Region,
};
use lucet_wasi::WasiCtxBuilder;
use lucetc::{HeapSettings, Bindings};
use lucetc::{Lucetc, LucetcOpts};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use tracing::{trace, instrument, Level};

mod util;
mod context;
use context::EngineContext;
use crate::topology::unit_test::build_unit_tests;
use std::fmt::Debug;

pub mod hostcall; // Pub is required for lucet.
mod defaults {
    use std::path::Path;

    pub(super) const BUILDER_CACHE_SIZE: usize = 50;
    pub(super) const ARTIFACT_CACHE: &str = "cache";
}

trait Engine {
    fn build(config: EngineConfig) -> Self;
    fn load<P>(&mut self, path: P) -> Result<()>
    where
        P: Into<PathBuf> + Debug;
    fn instantiate<P>(&mut self, path: P) -> Result<Uuid>
    where
        P: Into<PathBuf> + Debug;
    fn process(&mut self, id: &Uuid, events: Event) -> Result<Option<Event>>;
}

#[derive(Derivative, Clone, Debug)]
#[derivative(Default)]
struct EngineConfig {
    /// Since the engine may load or unload instances over the course of it's life, it uses an LRU
    /// cache to maintain instance builders.
    #[derivative(Default(value = "defaults::BUILDER_CACHE_SIZE"))]
    builder_cache_size: usize,
    #[derivative(Default(value = "defaults::ARTIFACT_CACHE.into()"))]
    artifact_cache: PathBuf,
}

#[instrument]
fn compile(input: impl AsRef<Path> + Debug, output: impl AsRef<Path> + Debug) -> Result<()> {
    event!(Level::TRACE, "compiling");

    let mut bindings = lucet_wasi::bindings();
    bindings.extend(&Bindings::from_str(include_str!("hostcall/bindings.json"))?)?;
    let ret = Lucetc::new(input)
        .with_bindings(bindings)
        .shared_object_file(output)?;

    event!(Level::TRACE, "compiled");
    Ok(ret)
}

#[derive(Derivative)]
#[derivative(Debug)]
struct DefaultEngine {
    /// A stored version of the config for later referencing.
    config: EngineConfig,
    /// Currently cached instance builders.
    #[derivative(Debug="ignore")]
    modules: LruCache<PathBuf, Arc<DlModule>>,
    /// Handles for instantiated instances.
    #[derivative(Debug="ignore")]
    instance_handles: BTreeMap<Uuid, InstanceHandle>,
}

impl Engine for DefaultEngine {
    #[instrument]
    fn build(config: EngineConfig) -> Self {
        event!(Level::TRACE, "building");

        lucet_wasi::export_wasi_funcs();
        let ret = Self {
            config: config.clone(),
            modules: LruCache::new(config.builder_cache_size),
            instance_handles: Default::default(),
        };

        event!(Level::TRACE, "built");
        ret
    }

    #[instrument]
    fn load<P>(&mut self, path: P) -> Result<()>
    where
        P: Into<PathBuf> + Debug,
    {
        event!(Level::TRACE, "loading");

        let path = path.into();
        let output_file = self
            .config
            .artifact_cache
            .join(path.file_stem().ok_or("Must load files")?)
            .with_extension("so");

        fs::create_dir_all(&self.config.artifact_cache)?;
        compile(&path, &output_file)?;
        // load the compiled Lucet module
        let dl_module = DlModule::load(&output_file).unwrap();
        self.modules.put(path, dl_module);

        event!(Level::TRACE, "loaded");
        Ok(())
    }

    #[instrument]
    fn instantiate<P>(&mut self, path: P) -> Result<Uuid>
    where
        P: Into<PathBuf> + Debug,
    {
        event!(Level::TRACE, "instantiating");

        let path = path.into();
        let module = self.modules.get(&path).ok_or("Could not load path")?;
        // create a new memory region with default limits on heap and stack size
        let region = &MmapRegion::create(1, &Limits {
            heap_memory_size: 16 * 64 * 1024 * 10, // 10MB
            ..Limits::default()
        })?;
        // instantiate the module in the memory region
        let instance = region.new_instance_builder(module.clone()).build()?;

        let id = uuid::Uuid::new_v4();
        self.instance_handles.insert(id.clone(), instance);

        event!(Level::TRACE, "instantiated");
        Ok(id)
    }

    #[instrument]
    fn process(&mut self, id: &Uuid, event: Event) -> Result<Option<Event>> {
        event!(Level::TRACE, "processing");

        let instance = self
            .instance_handles
            .get_mut(id)
            .ok_or("Could not load instance")?;

        // The instance context is essentially an anymap, so this these aren't colliding!
        let wasi_ctx = WasiCtxBuilder::new().inherit_stdio().build()?;
        instance.insert_embed_ctx(wasi_ctx);
        let engine_context = EngineContext::new(event);
        instance.insert_embed_ctx(engine_context);

        let worked = instance.run("process", &[])?;

        let engine_context: EngineContext = instance.remove_embed_ctx()
            .ok_or("Could not retrieve context after processing.")?;
        let EngineContext { event: out } = engine_context;

        event!(Level::TRACE, "processed");
        Ok(out)
    }
}

#[test]
fn protobuf() -> Result<()> {
    crate::test_util::trace_init();
    let module = "target/wasm32-wasi/release/protobuf.wasm";
    let mut engine = DefaultEngine::build(Default::default());
    let mut event = Event::new_empty_log();
    event.as_mut_log().insert("test", "testing");

    engine.load(module)?;
    let id = engine.instantiate(module)?;
    let out = engine.process(&id, event.clone())?;
    println!("{:#?}", out);
    Ok(())
}

// #[test]
// fn tester() {
//     lucet_wasi::export_wasi_funcs();
//     // let bindings = lucetc::Bindings::empty();
//     lucetc::Lucetc::new("untitled.wasm")
//         .with_bindings(lucet_wasi::bindings())
//         .shared_object_file("untitled.so")
//         .unwrap();
//     // ensure the WASI symbols are exported from the final executable
//     // load the compiled Lucet module
//     let dl_module = DlModule::load("untitled.so").unwrap();
//     // create a new memory region with default limits on heap and stack size
//     let region = MmapRegion::create(1, &Limits::default()).unwrap();
//     // instantiate the module in the memory region
//     let mut instance_builder = region.new_instance_builder(dl_module);
//     let mut instance = instance_builder.build().unwrap();
//     // prepare the WASI context, inheriting stdio handles from the host executable
//     // let wasi_ctx = WasiCtxBuilder::new().inherit_stdio().build().unwrap();
//     // instance.insert_embed_ctx(wasi_ctx);
//     // run the WASI main function
//     instance.run("test", &[]).unwrap();
// }
//
