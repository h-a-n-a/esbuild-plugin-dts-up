use std::collections::{HashMap, HashSet};
use std::sync::{
  atomic::{AtomicUsize, Ordering},
  Arc, Mutex,
};

use dashmap::DashSet;
use petgraph::{visit::EdgeRef, Direction};
use smol_str::SmolStr;
use tokio::sync::mpsc::{self, error::TryRecvError, Sender};

use crate::{
  ast::{
    self,
    module::{self, ModuleId},
  },
  graph::{
    async_worker::{AsyncWorker, WorkerMessage},
    ModuleEdge, ModuleGraph, ModuleIndex,
  },
  result::Error,
  utils::resolve_id,
};

#[derive(Debug)]
pub struct Graph {
  resolved_entry: ModuleId,
  entry_module_index: ModuleIndex,
  module_graph: ModuleGraph,
  id_to_module: HashMap<ModuleId, ast::module::Module>,
}

#[derive(Debug)]
pub struct GraphOptions<T: AsRef<str>> {
  pub entry: T,
}

impl Graph {
  pub fn new<T>(options: GraphOptions<T>) -> Self
  where
    T: AsRef<str>,
  {
    let resolved_entry = resolve_id(&nodejs_path::resolve!(options.entry.as_ref()));

    Self {
      resolved_entry,
      entry_module_index: Default::default(),
      id_to_module: Default::default(),
      module_graph: ModuleGraph::new(),
    }
  }

  pub async fn build(&mut self) -> Result<(), Error> {
    self.generate().await?;
    self.sort_modules();
    self.link_export_all();

    Ok(())
  }

  pub async fn generate(&mut self) -> Result<(), Error> {
    let num_of_threads = num_cpus::get_physical();
    let idle_thread_count: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(num_of_threads));

    let (tx, mut rx) = mpsc::channel::<WorkerMessage>(32);

    // TODO: replace with RwLock
    let modules_to_work: Arc<Mutex<Vec<ModuleId>>> =
      Arc::new(Mutex::new(vec![self.resolved_entry.clone()]));

    self.module_graph.add_module(self.resolved_entry.clone());

    let worked_modules: Arc<DashSet<ModuleId>> = Arc::new(DashSet::new());

    for _ in 0..num_of_threads {
      let idle_thread_count = idle_thread_count.clone();
      let mut async_worker = AsyncWorker {
        resp_tx: tx.clone(),
        modules_to_work: modules_to_work.clone(),
        worked_modules: worked_modules.clone(),
        resolved_entries: Arc::new(DashSet::from_iter(vec![self.resolved_entry.clone()])),
      };

      tokio::spawn(async move {
        loop {
          idle_thread_count.fetch_sub(1, Ordering::SeqCst);
          async_worker.run().await;
          idle_thread_count.fetch_add(1, Ordering::SeqCst);

          loop {
            if !async_worker.modules_to_work.lock().unwrap().is_empty() {
              break;
            } else if idle_thread_count.load(Ordering::SeqCst) == num_of_threads {
              return;
            }
          }
        }
      });
    }

    drop(tx);

    while let Some(worker_message) = rx.recv().await {
      use WorkerMessage::*;
      log::debug!("[AsyncWorker] Received new message -> {}", worker_message);
      match worker_message {
        NewModule(module) => {
          let id = module.id.clone();
          self.id_to_module.insert(id.clone(), module);
          let module_index = self.module_graph.get_or_add_module(id.clone());

          if id == self.resolved_entry {
            self.entry_module_index = module_index;
          }
        }
        NewDependency(from_id, to_id, edge) => {
          let from_module_index = self.module_graph.get_or_add_module(from_id);
          let to_module_index = self.module_graph.get_or_add_module(to_id);
          self
            .module_graph
            .add_edge(from_module_index, to_module_index, edge);
        }
      }
    }

    log::debug!("[Graph] generated module graph {:#?}", self.module_graph);

    Ok(())
  }

  fn link_export_all(&mut self) {
    self
      .get_sorted_modules()
      .clone()
      .into_iter()
      .for_each(|module_index| {
        let source_module_ids = self
          .module_graph
          .get_edges_directed(module_index, Direction::Incoming)
          .map(|edge| {
            (
              self.module_graph.get_module_id_by_index(&edge.source()),
              edge.weight().clone(),
            )
          })
          .collect::<Vec<_>>();

        source_module_ids.into_iter().for_each(|(module_id, edge)| {
          let module_exports = self
            .get_module_by_module_index_mut(&module_index)
            .exports
            .clone();
          let target_module_id = self.get_module_by_module_index(&module_index).id.clone();
          let dep_module = self.id_to_module.get_mut(&module_id).unwrap();

          match edge {
            ModuleEdge::ExportAll(_) => {
              module_exports
                .into_iter()
                .for_each(|(local_name, module_export)| {
                  log::debug!(
                    "[Graph] linking export all with identifier: `{}` from {} to {}",
                    local_name,
                    target_module_id,
                    module_id
                  );

                  match dep_module.exports.entry(local_name.clone()) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                      v.insert(module_export);
                    }
                    std::collections::hash_map::Entry::Occupied(o) => {
                      // TODO: should we eliminate the panic if local_name is defined at the same statement?
                      panic!("[Graph] duplicated key detected: {}", local_name);
                    }
                  }
                })
            }
            _ => (),
          }
        })
      })
  }

  fn sort_modules(&mut self) {
    self.module_graph.sort_modules(self.entry_module_index);
    log::debug!("[Graph] sorted modules {:#?}", self.get_sorted_modules());
  }

  fn get_module_by_module_index(&self, module_index: &ModuleIndex) -> &module::Module {
    let module_id = self.module_graph.get_module_id_by_index(module_index);
    self.id_to_module.get(&module_id).unwrap()
  }

  fn get_module_by_module_index_mut(&mut self, module_index: &ModuleIndex) -> &mut module::Module {
    let module_id = self.module_graph.get_module_id_by_index(module_index);
    // println!("id to module {:#?}", self.id_to_module);
    self.id_to_module.get_mut(&module_id).unwrap()
  }

  #[inline]
  fn get_sorted_modules(&self) -> Vec<ModuleIndex> {
    self.module_graph.get_sorted_modules().clone()
  }
}