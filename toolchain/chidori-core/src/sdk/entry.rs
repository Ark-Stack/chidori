use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId, MergedStateHistory};
use crate::execution::execution::execution_state::{ExecutionState, ExecutionStateEvaluation};
use crate::execution::execution::DependencyGraphMutation;
use crate::cells::{CellTypes, get_cell_name, LLMPromptCell};
use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::execution::primitives::serialized_value::{
    RkyvSerializedValue as RKV, RkyvSerializedValue,
};
use crate::sdk::md::{interpret_code_block, load_folder};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::{fmt, thread};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, mpsc, Mutex, MutexGuard};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::sleep;
use chumsky::prelude::any;
use petgraph::graphmap::DiGraphMap;
use serde::ser::SerializeMap;
use crate::execution::primitives::operation::OperationFnOutput;
use crate::utils::telemetry::{init_internal_telemetry, TraceEvents};


/// This is an SDK for building execution graphs. It is designed to be used interactively.

type Func = fn(RKV) -> RKV;

#[derive(PartialEq, Debug)]
enum PlaybackState {
    Paused,
    Running,
}

// TODO: set up a channel between the host and the instance
//     so that we can send events to instances while they run on another thread

/// Instanced environments are not Send and live on a single thread.
/// They execute their operations across multiple threads, but individual OperationNodes
/// must remain on the given thread they're initialized on.
pub struct InstancedEnvironment {
    env_rx: Receiver<UserInteractionMessage>,
    pub db: ExecutionGraph,
    execution_head_state_id: (usize, usize),
    playback_state: PlaybackState,
    runtime_event_sender: Option<Sender<EventsFromRuntime>>,
    trace_event_sender: Option<Sender<TraceEvents>>,
    shared_state: Arc<Mutex<SharedState>>
}

impl std::fmt::Debug for InstancedEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstancedEnvironment")
            .finish()
    }
}

impl InstancedEnvironment {
    fn new() -> InstancedEnvironment {
        let mut db = ExecutionGraph::new();
        let state_id = (0, 0);
        let playback_state = PlaybackState::Paused;
        // TODO: handle this better, this just makes our tests pass until its resolved
        let (tx, rx) = mpsc::channel();
        InstancedEnvironment {
            env_rx: rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: None,
            trace_event_sender: None,
            playback_state,
            shared_state: Arc::new(Mutex::new(SharedState::new())),
        }
    }

    // TODO: reload_cells needs to diff the mutations that live on the current branch, with the state
    //       that we see in the shared state when this event is fired.
    fn reload_cells(&mut self) -> anyhow::Result<()> {
        println!("Reloading cells");
        let cells_to_upsert: Vec<_> = {
            let shared_state = self.shared_state.lock().unwrap();
            shared_state.cells.iter().map(|cell| cell.clone()).collect()
        };

        let mut ids = vec![];
        for cell_holder in cells_to_upsert {
            if cell_holder.needs_update {
                ids.push(self.upsert_cell(cell_holder.cell.clone(), cell_holder.op_id)?);
            } else {
                // TODO: remove these unwraps and handle this better
                ids.push((cell_holder.applied_at.unwrap(), cell_holder.op_id.unwrap()));
            }
        }

        let mut shared_state = self.shared_state.lock().unwrap();
        for (i, cell) in shared_state.cells.iter_mut().enumerate() {
            let (applied_at, op_id) = ids[i];
            cell.applied_at = Some(applied_at);
            cell.op_id = Some(op_id);
            cell.needs_update = false;
        }

        if let Some(sender) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::CellsUpdated(shared_state.cells.clone())).unwrap();
        }
        Ok(())
    }

    // pub fn testing(&mut self, handle: tokio::runtime::Handle) -> anyhow::Result<()> {
    //     handle.spawn(async move {
    //         tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    //         anyhow::Ok(())
    //     });
    //     anyhow::Ok(())
    // }

    /// Entrypoint for execution of an instanced environment, handles messages from the host
    pub async fn run(&mut self) -> anyhow::Result<()> {
        self.playback_state = PlaybackState::Paused;

        // Reload cells to make sure we're up to date
        self.reload_cells();

        let _maybe_guard = self.trace_event_sender.as_ref().map(|sender| {
            tracing::subscriber::set_global_default(init_internal_telemetry(sender.clone()));
            tracing::subscriber::set_default(init_internal_telemetry(sender.clone()))
        });
        loop {
            if let Ok(message) = self.env_rx.try_recv() {
                match message {
                    UserInteractionMessage::Play => {
                        self.playback_state = PlaybackState::Running;
                    },
                    UserInteractionMessage::Pause => {
                        self.playback_state = PlaybackState::Paused;
                    },
                    UserInteractionMessage::ReloadCells => {
                        self.reload_cells();
                    },
                    UserInteractionMessage::RevertToState(id) => {
                        if let Some(id) = id {
                            self.execution_head_state_id = id;
                            let merged_state = self.db.get_merged_state_history(&id);
                            let sender = self.runtime_event_sender.as_mut().unwrap();
                            sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
                        }
                    },
                    _ => {}
                }
            }
            if self.playback_state == PlaybackState::Paused {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            } else {
                let output = self.step().await?;
                // If nothing happened, pause playback and wait for the user
                if output.is_empty() {
                    self.playback_state = PlaybackState::Paused;
                }
            }
        }
        Ok(())
    }

    pub fn get_state(&self) -> ExecutionState {
        match self.db.get_state_at_id(self.execution_head_state_id).unwrap() {
            ExecutionStateEvaluation::Complete(s) => s,
            ExecutionStateEvaluation::Executing(_) => ExecutionState::new()
        }
    }

    /// Increment the execution graph by one step
    #[tracing::instrument]
    pub(crate) async fn step(&mut self) -> anyhow::Result<Vec<(usize, OperationFnOutput)>> {
        println!("======================= Executing state with id {:?} ======================", self.execution_head_state_id);
        let ((state_id, state), outputs) = self.db.external_step_execution(self.execution_head_state_id).await?;
        if let Some(sender) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::ExecutionGraphUpdated(self.db.get_execution_graph_elements())).unwrap();
            sender.send(EventsFromRuntime::ExecutionStateChange(self.db.get_merged_state_history(&state_id))).unwrap();
        }
        println!("Resulted in state with id {:?}", &state_id);
        self.execution_head_state_id = state_id;
        Ok(outputs)
    }

    /// Add a cell into the execution graph
    #[tracing::instrument]
    pub fn upsert_cell(&mut self, cell: CellTypes, op_id: Option<usize>) -> anyhow::Result<(ExecutionNodeId, usize)> {
        println!("Upserting cell into state with id {:?}", &self.execution_head_state_id);
        let ((state_id, state), op_id) = self.db.mutate_graph(self.execution_head_state_id, cell, op_id)?;
        if let Some(sender) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::ExecutionStateChange(self.db.get_merged_state_history(&state_id))).unwrap();
            sender.send(EventsFromRuntime::DefinitionGraphUpdated(state.get_dependency_graph_flattened())).unwrap();
            sender.send(EventsFromRuntime::ExecutionGraphUpdated(self.db.get_execution_graph_elements())).unwrap();
        }
        self.execution_head_state_id = state_id;
        Ok((state_id, op_id))
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}

#[derive(Debug)]
pub enum UserInteractionMessage {
    Play,
    Pause,
    UserAction(String),
    RevertToState(Option<(usize, usize)>),
    ReloadCells,
    FetchCells,
    MutateCell
}


// https://github.com/rust-lang/rust/issues/22750
// TODO: we can't serialize these within the Tauri application due to some kind of issue
//       with serde versions once we introduced a deeper dependency on Deno.
//       we attempted the following patch to no avail:
//
//       [patch.crates-io]
//       deno = {path = "../../deno/cli"}
//       deno_runtime = {path = "../../deno/runtime"}
//       serde = {path = "../../serde/serde" }
//       serde_derive = {path = "../../serde/serde_derive" }
//       tauri = {path = "../../tauri/core/tauri" }
//
// TODO: in each of these we resolved to the same serde version.
//       we need to figure out how to resolve this issue, but to move forward
//       for now we will serialize these to Strings on this side of the interface
//       the original type of this object is as follows:
//
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EventsFromRuntime {
    DefinitionGraphUpdated(Vec<(OperationId, OperationId, Vec<DependencyReference>)>),
    ExecutionGraphUpdated(Vec<(ExecutionNodeId, ExecutionNodeId)>),
    ExecutionStateChange(MergedStateHistory),
    CellsUpdated(Vec<CellHolder>)
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct CellHolder {
    pub cell: CellTypes,
    pub op_id: Option<usize>,
    pub applied_at: Option<ExecutionNodeId>,
    needs_update: bool
}

#[derive(Debug)]
pub struct SharedState {
    latest_state: Option<ExecutionState>,
    cells: Vec<CellHolder>,
}


impl Serialize for SharedState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        let mut state = serializer.serialize_map(None)?;
        if let Some(map) = &self.latest_state {
            for (k, v) in &map.state {
                state.serialize_entry(&k, v.deref())?; // Dereference `Arc` to serialize the value inside
            }
        }
        state.end()
    }
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            latest_state: None,
            cells: vec![],
        }
    }
}


/// Chidori is the high level interface for interacting with our runtime.
/// It is responsible for loading cells and creating instances of the environment.
/// It is expected to run on a "main thread" while instances may run in background threads.
pub struct Chidori {

    /// Sender to push user requests to the instance, these events result in
    /// state changes within the instance
    instanced_env_tx: Option<Sender<UserInteractionMessage>>,

    /// Sender to pass changes in state within instances back to the main thread
    runtime_event_sender: Option<Sender<EventsFromRuntime>>,

    /// Sender to collect trace events from instances
    trace_event_sender: Option<Sender<TraceEvents>>,

    shared_state: Arc<Mutex<SharedState>>,
    pub loaded_path: Option<String>,
}

impl std::fmt::Debug for Chidori {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .finish()
    }
}

impl Chidori {
    pub fn new() -> Self {
        Chidori {
            instanced_env_tx: None,
            runtime_event_sender: None,
            trace_event_sender: None,
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                cells: vec![],
                latest_state: None,
            })),
        }
    }

    pub fn new_with_events(sender: Sender<TraceEvents>, runtime_event_sender: Sender<EventsFromRuntime>) -> Self {
        Chidori {
            instanced_env_tx: None,
            runtime_event_sender: Some(runtime_event_sender),
            trace_event_sender: Some(sender),
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                cells: vec![],
                latest_state: None,
            })),
        }
    }

    pub fn get_shared_state(&self) -> MutexGuard<'_, SharedState> {
        self.shared_state.lock().unwrap()
    }

    pub fn get_cells(&self) -> Vec<CellTypes> {
        vec![]
    }

    #[tracing::instrument]
    pub fn handle_user_action(&self, action: UserInteractionMessage) -> anyhow::Result<()> {
        if let Some(tx) = &self.instanced_env_tx {
            tx.send(action)?;
        }
        Ok(())
    }

    fn load_cells(&mut self, cells: Vec<CellTypes>) -> anyhow::Result<()>  {
        // TODO: this overrides the entire shared state object
        let cell_name_map = {
            let previous_cells = &self.shared_state.lock().unwrap().cells;
            previous_cells.iter().map(|cell| {
                let name = get_cell_name(&cell.cell);
                (name.clone(), cell.clone())
            }).collect::<HashMap<_, _>>()
        };

        let mut new_cells_state = vec![];
        for cell in cells {
            let name = get_cell_name(&cell);
            if let Some(prev_cell) = cell_name_map.get(&name) {
                if prev_cell.cell != cell {
                    new_cells_state.push(CellHolder {
                        cell,
                        applied_at: None,
                        op_id: prev_cell.op_id,
                        needs_update: true
                    });
                } else {
                    new_cells_state.push(prev_cell.clone());
                }
            } else {
                new_cells_state.push(CellHolder {
                    cell,
                    applied_at: None,
                    op_id: None,
                    needs_update: true
                });
            }
        }
        self.shared_state.lock().unwrap().cells = new_cells_state;
        println!("Cells commit to shared state");
        self.handle_user_action(UserInteractionMessage::ReloadCells)?;
        Ok(())
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<()> {
        let mut cells = vec![];
        crate::sdk::md::extract_code_blocks(s)
            .iter()
            .filter_map(|block| interpret_code_block(block))
            .for_each(|block| { cells.push(block); });
        self.loaded_path = Some("raw_text".to_string());
        self.load_cells(cells)
    }

    pub fn load_md_directory(&mut self, path: &Path) -> anyhow::Result<()> {
        let files = load_folder(path)?;
        let mut cells = vec![];
        for file in files {
            for block in file.result {
                if let Some(block) = interpret_code_block(&block) {
                    cells.push(block);
                }
            }
        }
        self.loaded_path = Some(path.to_str().unwrap().to_string());
        self.load_cells(cells)
    }

    pub fn get_instance(&mut self) -> anyhow::Result<InstancedEnvironment> {
        let (instanced_env_tx, env_rx) = mpsc::channel();
        self.instanced_env_tx = Some(instanced_env_tx);
        let mut db = ExecutionGraph::new();
        let state_id = (0, 0);
        let playback_state = PlaybackState::Paused;
        Ok(InstancedEnvironment {
            env_rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: self.runtime_event_sender.clone(),
            trace_event_sender: self.trace_event_sender.clone(),
            playback_state,
            shared_state: self.shared_state.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use tokio::runtime::Runtime;
    use crate::cells::{CodeCell, LLMPromptCell, LLMPromptCellChatConfiguration, SupportedLanguage, SupportedModelProviders, TextRange};
    use crate::utils;

    #[tokio::test]
    async fn test_execute_cells_with_global_dependency() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        x = 20
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_x, 0);
        let (_, op_id_y) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        y = x + 1
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_y, 1);
        // env.resolve_dependencies_from_input_signature();
        env.get_state().render_dependency_graph();
        env.step().await;
        assert_eq!(
            env.get_state().state_get(&op_id_x),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.get_state().state_get(&op_id_y), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&op_id_x), None);
        assert_eq!(
            env.get_state().state_get(&op_id_y),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_cells_between_code_and_llm() -> anyhow::Result<()> {
        dotenv::dotenv().ok();
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        x = "Here is a sample string"
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_x, 0);
        let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            function_invocation: false,
            configuration: LLMPromptCellChatConfiguration::default(),
            name: None,
            provider: SupportedModelProviders::OpenAI,
            req: "\
                      Say only a single word. Give no additional explanation.
                      What is the first word of the following: {{x}}.
                    "
                .to_string(),
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_y, 1);
        env.get_state().render_dependency_graph();
        env.step().await;
        assert_eq!(
            env.get_state().state_get(&op_id_x),
            Some(
                &RkyvObjectBuilder::new()
                    .insert_string("x", "Here is a sample string".to_string())
                    .build()
            )
        );
        assert_eq!(env.get_state().state_get(&op_id_y), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&op_id_x), None);
        assert_eq!(
            env.get_state().state_get(&op_id_y),
            Some(&RKV::String("Here".to_string()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_cells_via_prompt_calling_api() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        import chidori as ch
                        x = ch.prompt("generate_names", x="John")
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_x, 0);
        let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            function_invocation: false,
            configuration: LLMPromptCellChatConfiguration::default(),
            name: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            req: "\
                      Generate names starting with {{x}}
                    "
                .to_string(),
        }, TextRange::default()),
                                           None)?;
        assert_eq!(op_id_y, 1);
        env.get_state().render_dependency_graph();
        env.step().await;
        assert_eq!(
            env.get_state().state_get(&op_id_x),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.get_state().state_get(&op_id_y), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&op_id_x), None);
        assert_eq!(
            env.get_state().state_get(&op_id_y),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_cells_invoking_a_function() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, id) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        def add(x, y):
                            return x + y
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                      None)?;
        assert_eq!(id, 0);
        let (_, id) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            function_invocation: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        y = add(2, 3)
                        "#}),
        }, TextRange::default()),
                                      None)?;
        assert_eq!(id, 1);
        env.get_state().render_dependency_graph();
        env.step().await;
        // Empty object from the function declaration
        assert_eq!(
            env.get_state().state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.get_state().state_get(&1), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&0), None);
        assert_eq!(
            env.get_state().state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_inter_runtime_code() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, id) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        def add(x, y):
                            return x + y
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                      None)?;
        assert_eq!(id, 0);
        let (_, id) = env.upsert_cell(CellTypes::Code(CodeCell {
            name: None,
            function_invocation: None,
            language: SupportedLanguage::Deno,
            source_code: String::from(indoc! { r#"
                        const y = await add(2, 3);
                        "#}),
        }, TextRange::default()),
                                      None)?;
        assert_eq!(id, 1);
        env.get_state().render_dependency_graph();
        env.step().await;
        // Function declaration cell
        assert_eq!(
            env.get_state().state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.get_state().state_get(&1), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&0), None);
        assert_eq!(
            env.get_state().state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_dependencies_across_nodes() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            v = 40
            def sqrr(x):
                return x * x
            ```

            ```python
            y = v * 20
            z = sqrr(y)
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        env.step().await;
        // Function declaration cell
        assert_eq!(
            env.get_state().state_get(&0),
            Some(&RkyvObjectBuilder::new().insert_number("v", 40).build())
        );
        assert_eq!(env.get_state().state_get(&1), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&0), None);
        assert_eq!(
            env.get_state().state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("z", 640000).insert_number("y", 800).build())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_inter_runtime_code_md() {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            def add(x, y):
                return x + y
            ```

            ```javascript
            ---
            a: 2
            ---
            const y = add(2, 3);
            ```

            ```prompt (multi_prompt)
            Multiply {y} times {x}
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        let s = env.get_state();
        env.reload_cells();
        s.render_dependency_graph();
        env.step().await;
        // Function declaration cell
        assert_eq!(
            env.get_state().state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.get_state().state_get(&1), None);
        env.step().await;
        assert_eq!(env.get_state().state_get(&0), None);
        assert_eq!(
            env.get_state().state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
    }

    #[tokio::test]
    async fn test_execute_webservice_and_handle_request_with_code_cell_md() {
        // initialize tracing
        let _guard = utils::init_telemetry("http://localhost:7281").unwrap();

        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
                ```python
                def add(x, y):
                    return x + y
                ```

                ```web
                ---
                port: 3839
                ---
                POST / add [a, b]
                ```
                "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();

        // This will initialize the service
        env.step().await;
        env.step().await;
        env.step().await;

        // Function declaration cell
        let client = reqwest::Client::new();
        let mut payload = HashMap::new();
        payload.insert("a", 123);
        payload.insert("b", 456);

        let res = client.post(format!("http://127.0.0.1:{}", 3839))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(res.text().await.unwrap(), "579");
    }

    #[tokio::test]
    async fn test_execute_webservice_and_serve_html() {
        // initialize tracing
        let _guard = utils::init_telemetry("http://localhost:7281").unwrap();
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
                ```html (example)
                <div>Example</div>
                ```

                ```web
                ---
                port: 3838
                ---
                GET / example
                ```
                "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();

        // This will initialize the service
        env.step().await;
        env.step().await;
        env.step().await;

        let mut payload = HashMap::new();
        payload.insert("a", 123); // Replace 123 with your desired value for "a"
        payload.insert("b", 456); // Replace 456 with your desired value for "b"

        // Function declaration cell
        let client = reqwest::Client::new();
        let res = client.get(format!("http://127.0.0.1:{}", 3838))
            .send()
            .await
            .expect("Failed to send request");

        // TODO: why is this wrapped in quotes
        assert_eq!(res.text().await.unwrap(), "<div>Example</div>");
    }

    #[tokio::test]
    async fn test_core1_simple_math() -> anyhow::Result<()>{
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core1_simple_math")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await?;
        assert_eq!(out[0].1.output, RkyvObjectBuilder::new().insert_number("x", 20).build());
        let out = env.step().await?;
        assert_eq!(out[0].1.output, RkyvObjectBuilder::new().insert_number("y", 400).build());
        let out = env.step().await?;
        assert_eq!(out[0].1.output, RkyvObjectBuilder::new().insert_number("zj", 420).build());
        Ok(())
    }

    #[tokio::test]
    async fn test_core2_marshalling() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core2_marshalling")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let mut out = env.step().await?;
        out.sort_by(|a, b| b.0.cmp(&a.0));
        assert_eq!(out[0].0, 2);
        assert_eq!(out[0].1.output, RkyvObjectBuilder::new()
            .insert_object("y3", RkyvObjectBuilder::new()
                .insert_number("a", 1)
                .insert_number("b", 2)
                .insert_number("c", 3)
            )
            .insert_value("y2", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_number("y0", 1)
            .insert_number("y5", 1)
            .insert_value("y6", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_value("y1", RkyvSerializedValue::String("string".to_string()))
            .insert_value("y4", RkyvSerializedValue::Boolean(false))
            .build());
        assert_eq!(out[1].0, 0);
        assert_eq!(out[1].1.output, RkyvObjectBuilder::new()
            .insert_value("x2", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_object("x3", RkyvObjectBuilder::new()
                .insert_number("a", 1)
                .insert_number("b", 2)
                .insert_number("c", 3)
            )
            .insert_number("x0", 1)
            .insert_value("x5", RkyvSerializedValue::Float(1.0))
            .insert_value("x6", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_value("x1", RkyvSerializedValue::String("string".to_string()))
            .insert_value("x4", RkyvSerializedValue::Boolean(false))
            .insert_value("x7", RkyvSerializedValue::Set(HashSet::from_iter(vec![
                RkyvSerializedValue::String("c".to_string()),
                RkyvSerializedValue::String("a".to_string()),
                RkyvSerializedValue::String("b".to_string()),
            ].iter().cloned())))
            .build());
        let mut out = env.step().await?;
        assert_eq!(out[0].0, 3);
        assert!(out[0].1.stderr.contains(&"OK".to_string()));
        Ok(())
    }


    #[tokio::test]
    async fn test_core3_function_invocations() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core3_function_invocations")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        dbg!(env.step().await);
        dbg!(env.step().await);
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }

    #[tokio::test]
    async fn test_core4_async_function_invocations() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core4_async_function_invocations")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        env.step().await;
        env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }


    #[tokio::test]
    async fn test_core5_prompts_invoked_as_functions() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core5_prompts_invoked_as_functions")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }

    #[tokio::test]
    async fn test_core6_prompts_leveraging_function_calling() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core6_prompts_leveraging_function_calling")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }

    #[tokio::test]
    async fn test_core7_rag_stateful_memory_cells() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core7_rag_stateful_memory_cells")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }

    #[tokio::test]
    async fn test_core8_prompt_code_generation_and_execution() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core8_prompt_code_generation_and_execution")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }

    #[tokio::test]
    async fn test_core9_multi_agent_simulation() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core9_multi_agent_simulation")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state().have_all_operations_been_set_at_least_once(), true);
    }
}

