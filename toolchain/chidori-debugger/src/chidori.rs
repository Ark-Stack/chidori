use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use bevy::app::{App, Startup, Update};
use bevy::input::ButtonInput;
use bevy::prelude::{Commands, default, KeyCode, NextState, Res, ResMut, Resource};
use crate::bevy_egui::{EguiContexts};
use egui;
use egui::{Color32, Frame, Id, Margin, Response, RichText, vec2, Visuals, Widget};
use egui::panel::TopBottomSide;
use egui_tiles::{Tile, TileId};
use notify_debouncer_full::{
    DebounceEventResult,
    Debouncer,
    FileIdMap, new_debouncer, notify::{RecommendedWatcher, RecursiveMode, Watcher},
};
use chidori_core::cells::CellTypes;
use chidori_core::uuid::Uuid;

use chidori_core::execution::execution::execution_graph::{
    ExecutionNodeId, MergedStateHistory,
};
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::sdk::entry::{
    CellHolder, Chidori, EventsFromRuntime,
    UserInteractionMessage,
};
use chidori_core::tokio::task::JoinHandle;
use chidori_core::utils::telemetry::TraceEvents;

use crate::{GameState, tokio_tasks};

#[derive(Debug)]
pub struct Pane {
    pub tile_id: Option<TileId>,
    pub nr: String,
    pub rect: Option<egui::Rect>,
}

struct TreeBehavior {}

impl egui_tiles::Behavior<Pane> for TreeBehavior {
    fn tab_bar_color(&self, visuals: &Visuals) -> Color32 {
        Color32::from_hex("#1B1B1B").unwrap()
    }
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tile_id: egui_tiles::TileId,
        pane: &mut Pane,
    ) -> egui_tiles::UiResponse {
        pane.tile_id = Some(tile_id.clone());
        pane.rect = Some(ui.max_rect());
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        format!("{}", pane.nr).into()
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions {
            join_nested_linear_containers: true,
            prune_single_child_tabs: true,
            prune_empty_containers: true,
            prune_single_child_containers: true,
            prune_empty_tabs: true,
            all_panes_must_have_tabs: true,
            ..default()
        }
    }
}

#[derive(Resource, Default)]
pub struct EguiTreeIdentities {
    pub code_tile: Option<TileId>,
    pub logs_tile: Option<TileId>,
    pub graph_tile: Option<TileId>,
    pub traces_tile: Option<TileId>,
    pub chat_tile: Option<TileId>,
}

#[derive(Resource)]
pub struct EguiTree {
    pub tree: egui_tiles::Tree<Pane>,
}

fn keyboard_shortcut_tab_focus(
    mut identities: ResMut<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    button_input: Res<ButtonInput<KeyCode>>,
) {
    if button_input.pressed(KeyCode::SuperLeft) {
        if button_input.just_pressed(KeyCode::KeyT) {
            tree.tree.make_active(|id, _| {
                id == identities.traces_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyL) {
            tree.tree.make_active(|id, _| {
                id == identities.logs_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyG) {
            tree.tree.make_active(|id, _| {
                id == identities.graph_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyC) {
            tree.tree.make_active(|id, _| {
                id == identities.code_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyH) {
            tree.tree.make_active(|id, _| {
                id == identities.chat_tile.unwrap()
            });
        }
    }

}


fn maintain_egui_tree_identities(
    mut identities: ResMut<EguiTreeIdentities>,
    tree: ResMut<EguiTree>
) {
    tree.tree.tiles.iter().for_each(|(tile_id, tile)| {
        match tile {
            Tile::Pane(p) => {
                if &p.nr == &"Code" {
                    identities.code_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Logs" {
                    identities.logs_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Graph" {
                    identities.graph_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Traces" {
                    identities.traces_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Chat" {
                    identities.chat_tile = Some(tile_id.clone());
                }
            }
            _ => {}
        }
    })

}

impl Default for EguiTree {
    fn default() -> Self {
        let mut next_view_nr = 0;
        let mut gen_pane = |name: String| {
            let pane = Pane {
                tile_id: None,
                nr: name,
                rect: None,
            };
            next_view_nr += 1;
            pane
        };

        let mut tiles = egui_tiles::Tiles::default();

        let tabs = vec![
            tiles.insert_pane(gen_pane(String::from("Code"))),
            tiles.insert_pane(gen_pane(String::from("Logs"))),
            tiles.insert_pane(gen_pane(String::from("Graph"))),
            tiles.insert_pane(gen_pane(String::from("Traces"))),
            tiles.insert_pane(gen_pane(String::from("Chat")))
        ];
        let root = tiles.insert_tab_tile(tabs);

        EguiTree {
            tree: egui_tiles::Tree::new("my_tree", root, tiles),
        }
    }
}

const EXAMPLES_CORE1: &str = include_str!("../../chidori-core/examples/core1_simple_math/core.md");
const EXAMPLES_CORE2: &str = include_str!("../../chidori-core/examples/core2_marshalling/core.md");
const EXAMPLES_CORE3: &str =
    include_str!("../../chidori-core/examples/core3_function_invocations/core.md");
const EXAMPLES_CORE4: &str =
    include_str!("../../chidori-core/examples/core4_async_function_invocations/core.md");
const EXAMPLES_CORE5: &str =
    include_str!("../../chidori-core/examples/core5_prompts_invoked_as_functions/core.md");
const EXAMPLES_CORE6: &str =
    include_str!("../../chidori-core/examples/core6_prompts_leveraging_function_calling/core.md");
const EXAMPLES_CORE7: &str =
    include_str!("../../chidori-core/examples/core7_rag_stateful_memory_cells/core.md");
const EXAMPLES_CORE8: &str =
    include_str!("../../chidori-core/examples/core8_prompt_code_generation_and_execution/core.md");
const EXAMPLES_CORE9: &str =
    include_str!("../../chidori-core/examples/core9_multi_agent_simulation/core.md");

#[derive(Resource)]
pub struct ChidoriTraceEvents {
    pub inner: Vec<TraceEvents>,
}

#[derive(Resource)]
pub struct ChidoriExecutionIdsToStates {
    pub inner: HashMap<ExecutionNodeId, ExecutionState>,
}

#[derive(Resource)]
pub struct ChidoriExecutionGraph {
    pub inner: Vec<(ExecutionNodeId, ExecutionNodeId)>,
    pub current_execution_head: ExecutionNodeId,
}

#[derive(Resource)]
pub struct ChidoriDefinitionGraph {
    pub inner: Vec<(OperationId, OperationId, Vec<DependencyReference>)>,
}

#[derive(Resource)]
pub struct ChidoriExecutionState {
    pub inner: Option<MergedStateHistory>,
}


#[derive(Resource, Default)]
pub struct ChidoriLogMessages {
    pub inner: Vec<String>,
}

#[derive(Resource)]
pub struct ChidoriCells {
    pub editor_cells: Vec<CellHolder>,
    pub state_cells: Vec<CellTypes>,
}

#[derive(Resource)]
pub struct InternalState {
    watched_path: Mutex<Option<String>>,
    file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,
    background_thread: Mutex<Option<JoinHandle<()>>>,
    pub chidori: Arc<Mutex<Chidori>>,
    display_example_modal: bool,
}

impl InternalState {
    pub fn get_loaded_path(&self) -> String {
        let env = self.chidori.lock().unwrap();
        if env.loaded_path.is_none() {
            return "".to_string();
        }
        env.loaded_path.as_ref().unwrap().to_string()
    }

    pub fn move_state_view_to_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::RevertToState(Some(id)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_current_execution_head(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        Ok(())
    }

    pub fn get_execution_state_at_id(
        &self,
        execution_node_id: ExecutionNodeId,
    ) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::FetchStateAt(execution_node_id))
            .map_err(|e| e.to_string())?;
        Ok(())
    }


    pub fn step(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Step)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn play(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Play)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pause(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Pause)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_execution_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::RevertToState(Some(id)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_string(&mut self, file: &str) -> anyhow::Result<(), String> {
        self.display_example_modal = false;
        let chidori = self.chidori.clone();
        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            chidori_guard.load_md_string(file).expect("Failed to load markdown string");
        }
        Ok(())
    }

    pub fn load_and_watch_directory(&self, path: String) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        let mut file_watch_guard = self.file_watch.lock().expect("Failed to lock file_watch");

        // Initialize the watcher and set up the event handler within a single block to avoid cloning `path` multiple times.
        let watcher_chidori = chidori.clone();
        let watcher_path = path.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(200),
            None,
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => events.iter().for_each(|event| {}),
                    Err(errors) => errors.iter().for_each(|error| {}),
                }
                let path_buf = PathBuf::from(&watcher_path);
                let mut chidori_guard = watcher_chidori.lock().expect("Failed to lock chidori");
                chidori_guard.load_md_directory(&path_buf).expect("Failed to load markdown directory");
            },
        )
        .unwrap();

        // Watch the directory for changes. Since `path` has not been moved, we can reuse it here.
        debouncer
            .watcher()
            .watch(Path::new(&path), RecursiveMode::Recursive)
            .expect("Failed to watch directory");
        debouncer
            .cache()
            .add_root(Path::new(&path), RecursiveMode::Recursive);

        // Replace the old watcher with the new one.
        *file_watch_guard = Some(debouncer);

        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            dbg!("Loading directory");
            chidori_guard
                .load_md_directory(Path::new(&path))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn setup(mut commands: Commands, runtime: ResMut<tokio_tasks::TokioTasksRuntime>) {
    commands.insert_resource(ChidoriTraceEvents { inner: vec![] });
    commands.insert_resource(ChidoriExecutionGraph {
        inner: vec![],
        current_execution_head: Uuid::nil(),
    });
    commands.insert_resource(ChidoriExecutionIdsToStates {
        inner: HashMap::new(),
    });

    commands.insert_resource(ChidoriDefinitionGraph { inner: vec![] });
    commands.insert_resource(ChidoriExecutionState { inner: None });
    commands.insert_resource(ChidoriCells { editor_cells: vec![], state_cells: vec![] });

    let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
    let mut internal_state = InternalState {
        chidori: Arc::new(Mutex::new(Chidori::new_with_events(
            trace_event_sender,
            runtime_event_sender,
        ))),
        watched_path: Mutex::new(None),
        background_thread: Mutex::new(None),
        file_watch: Mutex::new(None),
        display_example_modal: true,
    };

    {
        let mut background_thread_guard = internal_state
            .background_thread
            .lock()
            .expect("Failed to lock background_thread");
        let chidori = internal_state.chidori.clone();
        *background_thread_guard = Some(runtime.spawn_background_task(|mut ctx| async move {
            loop {
                // Create an instance within the loop
                let mut instance = {
                    let mut chidori_guard = chidori.lock().unwrap();
                    let instance = chidori_guard.get_instance().unwrap();
                    drop(chidori_guard); // Drop the lock on chidori to avoid deadlock
                    instance
                };

                let await_ready = instance.wait_until_ready().await;
                let result = instance.run().await;
                match result {
                    Ok(_) => {
                        panic!("Instance completed execution and closed successfully.");
                        break;
                    }
                    Err(e) => {
                        panic!("Error occurred: {}, retrying...", e);
                    }
                }
            }
        }));
    }

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match runtime_event_receiver.try_recv() {
                Ok(msg) => {
                    println!("Received from runtime: {:?}", &msg);
                    match msg {
                        EventsFromRuntime::ExecutionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionGraph>()
                                {
                                    s.inner = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::StateAtId(id, state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionIdsToStates>()
                                {
                                    s.inner.insert(id, state);
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ExecutionStateChange(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionState>()
                                {
                                    s.inner = Some(state);
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::DefinitionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriDefinitionGraph>()
                                {
                                    s.inner = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::CellsUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriCells>() {
                                    s.editor_cells = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::UpdateExecutionHead(head) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionGraph>()
                                {
                                    s.current_execution_head = head;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ReceivedChatMessage(_) => {}
                        EventsFromRuntime::ExecutionStateCellsViewUpdated(cells) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriCells>() {
                                    s.state_cells = cells;
                                }
                            })
                                .await;

                        }
                    }
                }
                Err(e) => match e {
                    TryRecvError::Empty => {}
                    TryRecvError::Disconnected => {
                        println!("Runtime channel disconnected");
                        break;
                    }
                },
            }
        }
        println!("Runtime event loop ended");
    });

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match trace_event_receiver.recv() {
                Ok(msg) => {
                    // println!("Received: {:?}", &msg);
                    // handle.emit_all("execution:events", Some(trace_event_to_string(msg)));
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriTraceEvents>() {
                            s.inner.push(msg);
                        }
                    })
                    .await;
                }
                Err(_) => {
                    println!("Channel closed");
                    break;
                }
            }
        }
    });

    commands.insert_resource(internal_state);
}

fn with_cursor(res: Response) -> Response {
    if res.hovered() {
        res.ctx.output_mut(|p| {
            p.cursor_icon = egui::CursorIcon::PointingHand;
        });
    }
    res
}

pub fn update_gui(
    mut commands: Commands,
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<InternalState>,
    mut state: ResMut<NextState<GameState>>,
) {
    if internal_state.display_example_modal {
        render_example_selection_modal(&mut contexts, &mut internal_state);
    } else {
        egui::CentralPanel::default()
            .frame(egui::Frame::default().outer_margin(Margin {
                left: 0.0,
                right: 0.0,
                top: 40.0,
                bottom: 0.0,
            }))
            .show(contexts.ctx_mut(), |ui| {
                let mut behavior = TreeBehavior {};
                egui_tree.tree.ui(&mut behavior, ui);
            });
    }

    egui::TopBottomPanel::new(TopBottomSide::Top, Id::new("top_panel")).show(
        contexts.ctx_mut(),
        |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
            // ui.text_edit_multiline(&mut text);
            // a simple button opening the dialog
            let mut frame = egui::Frame::default()
                .inner_margin(Margin::symmetric(8.0, 4.0))
                .begin(ui);
            {
                let mut ui = &mut frame.content_ui;
                ui.horizontal(|ui| {
                    ui.style_mut().spacing.item_spacing = egui::vec2(32.0, 8.0);
                    if with_cursor(ui.button("Open")).clicked() {
                        // let sender = self.text_channel.0.clone();
                        runtime.spawn_background_task(|mut ctx| async move {
                            let task = rfd::AsyncFileDialog::new().pick_file();
                            let file = task.await;
                            if let Some(file) = file {
                                let text = file.read().await;
                                // let _ = sender.send(String::from_utf8_lossy(&text).to_string());
                                // ctx.request_repaint();
                            }
                        });
                    }
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);

                    if with_cursor(ui.button("Run")).clicked() {
                        internal_state.play();
                    }
                    if with_cursor(ui.button("Stop")).clicked() {
                        internal_state.pause();
                    }
                    if with_cursor(ui.button("Step")).clicked() {
                        internal_state.step();
                    }
                });
            }
            frame.end(ui);
        },
    );
}

fn render_example_selection_modal(mut contexts: &mut EguiContexts, mut internal_state: &mut ResMut<InternalState>) {
    egui::CentralPanel::default()
        .frame(
            Frame::default()
                .fill(Color32::from_hex("#222222").unwrap())
                .inner_margin(16.0)
                .outer_margin(100.0)
                .rounding(5.0),
        )
        .show(contexts.ctx_mut(), |ui| {
            ui.heading("Chidori");
            ui.add_space(12.0);
            let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
                ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 12.0);
                // Add widgets inside the frame
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label("Load Example:");
                        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
                        let buttons_text_load = vec![
                            ("Core 1: Simple Math", EXAMPLES_CORE1),
                            ("Core 2: Marshalling", EXAMPLES_CORE2),
                            ("Core 3: Function Invocations", EXAMPLES_CORE3),
                            ("Core 4: Async Function Invocations", EXAMPLES_CORE4),
                            ("Core 5: Prompts Invoked as Functions", EXAMPLES_CORE5),
                            (
                                "Core 6: Prompts Leveraging Function Calling",
                                EXAMPLES_CORE6,
                            ),
                            ("Core 7: Rag Stateful Memory Cells", EXAMPLES_CORE7),
                            (
                                "Core 8: Prompt Code Generation and Execution",
                                EXAMPLES_CORE8,
                            ),
                            ("Core 9: Multi-Agent Simulation", EXAMPLES_CORE9),
                        ];
                        for button in buttons_text_load {
                            let res = with_cursor(ui.button(button.0));
                            if res.clicked() {
                                internal_state.load_string(button.1);
                            }
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::Image::new(egui::include_image!("../assets/images/tblogo-white.png"))
                            .fit_to_exact_size(vec2(512.0, 512.0))
                            .rounding(5.0)
                            .ui(ui);
                    });
                });
            }
            frame.end(ui);
        });
}

pub fn chidori_plugin(app: &mut App) {
    app.init_resource::<EguiTree>()
        .init_resource::<EguiTreeIdentities>()
        .init_resource::<ChidoriLogMessages>()
        .add_systems(Update, (update_gui, maintain_egui_tree_identities))
        .add_systems(Startup, setup);
}
