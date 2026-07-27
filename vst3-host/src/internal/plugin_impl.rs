//! Internal VST3 plugin implementation

use crate::{
    audio::AudioBuffers,
    error::{Error, Result},
    midi::{MidiChannel, MidiEvent},
    parameters::{Parameter, ParameterChange},
    plugin::{PluginInfo, PluginInternal},
};
use crossbeam_queue::ArrayQueue;
use std::ptr;
use std::sync::{Arc, Mutex};
use vst3::Steinberg::Vst::BusDirections_::*;
use vst3::Steinberg::Vst::Event_::EventTypes_::*;
use vst3::Steinberg::Vst::MediaTypes_::*;
use vst3::Steinberg::{IPlugView, IPlugViewTrait};
use vst3::{ComPtr, ComWrapper, Interface, Steinberg::Vst::*, Steinberg::*};

#[cfg(target_os = "linux")]
use super::com_implementations::RunLoopRegistry;
use super::{
    com_implementations::{
        create_event_list, create_host_application, create_host_plug_frame, create_memory_stream,
        create_memory_stream_from, ComponentHandler, HostApplication, HostEventList, HostPlugFrame,
        ParameterChanges, MAX_EDITOR_FEEDBACK, MAX_QUEUED_EVENTS,
    },
    module_loader::{load_module, VstModule},
};

/// Cap on the buffered output MIDI a plugin emits, so a host that never polls can't grow it
/// forever. The buffer is pre-reserved to this size so steady-state pushes never reallocate.
const MAX_OUTPUT_MIDI: usize = 4096;

/// Cap on simultaneously tracked `note_on` ids (see `PluginImpl::active_notes`). Far above any
/// real polyphony, and bounds a caller that starts notes it never releases.
const MAX_TRACKED_NOTES: usize = 1024;

/// Cap on parameter changes queued for the next block (see `PluginImpl::pending_param_changes`).
/// The queue's only drain is `process()`, which returns early while the plugin isn't processing —
/// so a host whose stream keeps running after `stop_processing`, or one automating parameters
/// before playback starts, would otherwise grow it forever and then flood a single block with the
/// whole backlog. The sibling of `MAX_QUEUED_EVENTS` on the MIDI side: pre-reserved so the audio
/// path never reallocates, and changes past the cap are dropped (and counted).
const MAX_PENDING_PARAM_CHANGES: usize = 4096;

/// Internal plugin implementation that handles all VST3 COM interactions
pub struct PluginImpl {
    // Core VST3 interfaces
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// True when the component and controller are the same object (single-component
    /// plugin). Then `IComponent::setState` already restores the controller, and calling
    /// `setComponentState` on top of it would double-apply and corrupt parameters.
    single_component: bool,

    // Plugin metadata
    pub(crate) info: PluginInfo,

    // Processing state
    is_active: bool,
    is_processing: bool,
    sample_rate: f64,
    block_size: usize,
    /// The configuration the plugin's last successful `setupProcessing` carried, so
    /// `start_processing` can tell a re-start (nothing to do) from a configuration change
    /// (which has to be applied while the component is inactive). `None` until the first setup.
    applied_setup: Option<AppliedSetup>,
    /// Transport tempo (BPM) advertised in the host `ProcessContext`.
    tempo: f64,
    /// Time signature numerator advertised in the host `ProcessContext`.
    time_sig_numerator: i32,
    /// Time signature denominator advertised in the host `ProcessContext`.
    time_sig_denominator: i32,
    /// Whether the transport is playing (the `kPlaying` flag in `ProcessContext.state`).
    playing: bool,
    /// Real-time vs offline processing, baked into `ProcessSetup`/`process_data` at setup.
    process_mode: crate::plugin::ProcessMode,
    /// Monotonic allocator for per-voice note ids (note_on); 0/-1 reserved for "unset".
    next_note_id: i32,
    // `(note_id, channel, pitch)` for notes started via `note_on`, so `note_off` can address the
    // release by pitch as well as by id. Pre-reserved and capped, so tracking never allocates on
    // the audio thread and a caller that never releases its notes can't grow it without bound.
    active_notes: Vec<(i32, i16, i16)>,

    // Host data structures
    process_data: Option<Box<HostProcessData>>,
    component_handler: Option<ComWrapper<ComponentHandler>>,

    // Parameter changes queued by the host (set_parameter / automation) to be fed into the
    // processor's input parameter queue at the start of the next process() block. Serialized
    // with process() by the caller's &mut access, so a plain Vec (no lock) is sufficient.
    // Pre-reserved to MAX_PENDING_PARAM_CHANGES and capped there — see the constant.
    pending_param_changes: Vec<ParameterChange>,
    // How many parameter changes have been dropped because the queue was full. Reported in the
    // warning so a host sees a running total rather than one line per lost change.
    dropped_param_changes: u64,

    // Parameter edits the plugin's *own editor* reported via `IComponentHandler::performEdit`,
    // after `process()` has routed them into the processor's input queue. Drained by the host
    // via `get_parameter_changes()` to update its UI. Separate from the raw performEdit sink
    // (`component_handler.parameter_changes`) so feeding the DSP and updating the display are
    // not two consumers racing to drain the same buffer.
    gui_param_changes_for_host: Arc<Mutex<Vec<(u32, f64)>>>,

    // Event handling
    input_events: ComWrapper<HostEventList>,
    output_events: ComWrapper<HostEventList>,
    // Holds a block's queued input events while `process` distributes them over the block's
    // chunks: each chunk is staged into `input_events` with only the events that fall inside it.
    // Pre-reserved to MAX_QUEUED_EVENTS (the input list's own cap), so splitting a block never
    // allocates on the audio thread.
    chunk_events: Vec<Event>,
    // MIDI the plugin has emitted (captured from output_events after each process block,
    // converted to MidiEvent), buffered for the host to poll. A lock-free bounded queue so the
    // audio thread can push without locking and a UI thread can drain concurrently; when full
    // the oldest event is dropped (bounded memory if the host never polls).
    output_midi: Arc<ArrayQueue<MidiEvent>>,

    // Plugin view
    plugin_view: Option<ComPtr<IPlugView>>,

    // Editor resize plumbing: the IPlugFrame handed to the plugin's view, and the slot it
    // writes requested sizes into (drained via take_editor_resize_request).
    plug_frame: ComWrapper<HostPlugFrame>,
    editor_resize: Arc<Mutex<Option<(i32, i32)>>>,
    // Linux IRunLoop registrations from the plugin's editor (fd handlers +
    // timers), serviced on the host UI thread via `service_run_loop`.
    #[cfg(target_os = "linux")]
    run_loop: Arc<Mutex<RunLoopRegistry>>,

    // Host application context passed to initialize() — kept alive for the plugin's
    // lifetime because the plugin may retain a reference to it.
    _host_app: ComWrapper<HostApplication>,

    // VST3 module handle (kept alive)
    _module: Box<dyn VstModule>,
}

// Processing data structure
struct HostProcessData {
    process_data: ProcessData,
    input_buffers: Vec<Vec<f32>>,
    output_buffers: Vec<Vec<f32>>,
    input_bus_buffers: Vec<AudioBusBuffers>,
    output_bus_buffers: Vec<AudioBusBuffers>,
    process_context: ProcessContext,
    input_param_changes: ComWrapper<ParameterChanges>,
    output_param_changes: ComWrapper<ParameterChanges>,
    // Preallocated channel-pointer arrays, built once in prepare_buffers. The audio buffers'
    // addresses are stable after allocation, so process() reuses these instead of rebuilding
    // them every block — keeping the steady-state audio path allocation-free.
    input_channel_ptrs: SendChannelPtrs,
    output_channel_ptrs: SendChannelPtrs,
}

/// The processing configuration handed to the plugin by `setupProcessing`, cached so a
/// re-start can tell "nothing changed" (skip setup, the plugin still has it) from "the host
/// reconfigured me" (setup must be re-run, and only while the component is inactive).
#[derive(Debug, Clone, Copy, PartialEq)]
struct AppliedSetup {
    sample_rate: f64,
    block_size: usize,
    /// The VST3 `ProcessModes` value, not the safe enum — this is compared against what was
    /// actually written into `ProcessSetup`.
    process_mode: i32,
}

/// Per-bus channel pointers into the (audio-thread-owned) audio buffers.
///
/// `Send` because the pointers are only ever dereferenced on the one thread that owns the
/// `HostProcessData` (and thus the buffers they point into); the `Plugin` is moved to that
/// thread as a unit. The raw pointers never escape to another thread.
struct SendChannelPtrs(Vec<Vec<*mut f32>>);
unsafe impl Send for SendChannelPtrs {}

impl PluginImpl {
    /// Configure the transport advertised to the plugin in the host `ProcessContext`.
    ///
    /// Call before processing starts (the values are baked into the context when
    /// `create_process_data` runs during `start_processing`). The musical playhead
    /// (`projectTimeMusic`) derives from `tempo` as the transport advances.
    pub fn set_transport(
        &mut self,
        tempo: f64,
        time_sig_numerator: i32,
        time_sig_denominator: i32,
    ) {
        self.tempo = tempo;
        self.time_sig_numerator = time_sig_numerator;
        self.time_sig_denominator = time_sig_denominator;
    }

    /// Update the transport tempo for the **next** processed block, even while processing is
    /// active: the stored tempo (used to rebuild the context after a reconfigure) and the live
    /// `ProcessContext` both move, and the musical playhead derives from the new tempo.
    fn update_tempo(&mut self, bpm: f64) {
        self.tempo = bpm;
        if let Some(ref mut data) = self.process_data {
            data.process_context.tempo = bpm;
        }
    }

    /// Update the transport time signature for the **next** processed block, even while
    /// processing is active (stored fields plus the live `ProcessContext`).
    fn update_time_signature(&mut self, numerator: i32, denominator: i32) {
        self.time_sig_numerator = numerator;
        self.time_sig_denominator = denominator;
        if let Some(ref mut data) = self.process_data {
            data.process_context.timeSigNumerator = numerator;
            data.process_context.timeSigDenominator = denominator;
        }
    }

    /// Toggle the transport playing state (`kPlaying`) for the **next** processed block, even
    /// while processing is active (stored field plus the live `ProcessContext.state`).
    fn update_playing(&mut self, playing: bool) {
        self.playing = playing;
        if let Some(ref mut data) = self.process_data {
            data.process_context.state = process_context_state(playing);
        }
    }

    /// Apply the host-configured sample rate / block size before processing starts. Called at
    /// load so `setupProcessing` (which runs at `start_processing`) uses the builder's settings
    /// rather than the internal defaults.
    pub fn set_audio_config(&mut self, sample_rate: f64, block_size: usize) {
        self.sample_rate = sample_rate;
        self.block_size = block_size;
    }

    /// Get parameter changes the plugin's editor made (for the host to update its UI).
    ///
    /// Returns edits that `process()` has already routed into the processor's input queue, so
    /// the DSP and the host display stay in sync. (Before processing has started, falls back to
    /// the raw performEdit sink so edits aren't lost.)
    pub fn get_parameter_changes(&self) -> Vec<(u32, f64)> {
        // Both drains take the elements in place rather than `mem::take`-ing the `Vec`: taking it
        // would leave a zero-capacity buffer behind, so the next block's `append` would
        // reallocate — on the audio thread, for the stash.
        if let Ok(mut stash) = self.gui_param_changes_for_host.lock() {
            if !stash.is_empty() {
                return stash.drain(..).collect();
            }
        }
        // Not processing yet (process() hasn't run to move edits into the stash): drain the raw
        // performEdit sink directly so the host UI still reflects editor changes.
        if !self.is_processing {
            if let Some(ref handler) = self.component_handler {
                if let Ok(mut changes) = handler.parameter_changes.lock() {
                    return changes.drain(..).collect();
                }
            }
        }
        Vec::new()
    }

    /// Load a VST3 plugin from the given path
    pub fn load(path: &std::path::Path) -> Result<Self> {
        unsafe {
            log::info!("=== PLUGIN LOADING START ===");
            log::info!("Loading plugin from: {}", path.display());

            // Load the VST3 module using platform-specific loader. Declared first so it drops
            // LAST: everything below lives in the module's address space, and the COM teardown
            // (whether via `InitializedComponent` or `Drop for PluginImpl`) must complete before
            // the module unmaps.
            log::debug!("Step 1: Loading VST3 module...");
            let module = load_module(path)?;
            log::debug!("VST3 module loaded successfully");

            // Get factory from module
            log::debug!("Step 2: Getting factory from module...");
            let factory_ptr = module.get_factory()?;
            log::debug!("Factory obtained, ptr: {:?}", factory_ptr);
            if factory_ptr.is_null() {
                return Err(Error::PluginLoadFailed(
                    "GetPluginFactory returned null".to_string(),
                ));
            }

            log::debug!("Step 3: Wrapping factory in ComPtr...");
            let factory = ComPtr::<IPluginFactory>::from_raw(factory_ptr).ok_or_else(|| {
                Error::PluginLoadFailed("Failed to create factory ComPtr".to_string())
            })?;
            log::debug!("Factory wrapped successfully");

            // Find and create the audio component
            log::debug!("Step 4: Creating audio component...");
            let (component, class_index) = Self::create_component(&factory)?;
            log::debug!("Component created successfully (class index {class_index})");

            // Initialize component with a host-application context. Passing null here
            // crashes plugins that query the host (u-he, Waves, ...); see HostApplication.
            log::debug!("Step 5: Initializing component...");
            let host_app = create_host_application();
            let host_ctx = host_app.to_com_ptr::<IHostApplication>();
            let context = host_ctx
                .as_ref()
                .map(|p| p.as_ptr() as *mut FUnknown)
                .unwrap_or(ptr::null_mut());
            let init_result = component.initialize(context);
            if init_result != kResultOk {
                // A component that failed to initialize must NOT be terminated (terminate pairs
                // with a *successful* initialize), and must certainly not be driven on through
                // activateBus/setActive/process — so bail before anything else touches it. The
                // ComPtr releases it here and the module unloads as this frame unwinds.
                return Err(Error::PluginLoadFailed(format!(
                    "IComponent::initialize failed: {init_result:#x}"
                )));
            }
            log::debug!("Component initialized with result: {init_result:#x}");

            // The component is live from here: it may have spawned threads and registered
            // callbacks, so every failure below has to run the full ordered teardown before the
            // module unloads. The guard does that until the finished `PluginImpl` takes over.
            let mut initialized = InitializedComponent::new(component.clone());

            // CRITICAL: Activate event buses for MIDI processing
            log::debug!("Step 6: Activating event buses...");
            Self::activate_event_buses(&component)?;
            log::debug!("Event buses activated");

            // Get processor interface
            log::debug!("Step 7: Getting IAudioProcessor interface...");
            let processor = component.cast::<IAudioProcessor>().ok_or_else(|| {
                Error::InterfaceError("Component does not implement IAudioProcessor".to_string())
            })?;
            log::debug!("IAudioProcessor interface obtained");

            // Create component handler for parameter change notifications
            log::debug!("Step 8: Creating component handler...");
            let parameter_changes = Arc::new(Mutex::new(Vec::with_capacity(MAX_EDITOR_FEEDBACK)));
            let component_handler =
                ComWrapper::new(ComponentHandler::new(parameter_changes.clone()));
            log::debug!("Component handler created");

            // Get or create controller (handles both single-component and separate controller)
            log::debug!("Step 9: Getting or creating controller...");
            // A component that directly implements IEditController is a single-component
            // plugin; this distinction matters for state restore (see `single_component`).
            let single_component = component.cast::<IEditController>().is_some();
            let controller = Self::get_or_create_controller(&component, &factory, context)?;
            initialized.attach_controller(controller.clone(), single_component);
            log::debug!(
                "Controller obtained: {} (single_component: {single_component})",
                controller.is_some()
            );

            // Connect component and controller if they are separate
            if let Some(ref ctrl) = controller {
                log::debug!("Step 10: Connecting component and controller...");
                Self::connect_component_and_controller(&component, ctrl)?;
                log::debug!("Component and controller connected");

                // Set component handler on controller for parameter change notifications
                log::debug!("Step 11: Setting component handler on controller...");
                if let Some(handler_ptr) = component_handler.as_com_ref::<IComponentHandler>() {
                    let result = ctrl.setComponentHandler(handler_ptr.as_ptr());
                    if result == kResultOk {
                        log::debug!("Component handler set on controller successfully");
                    } else {
                        log::warn!(
                            "Failed to set component handler on controller: {:#x}",
                            result
                        );
                    }
                } else {
                    log::error!("Failed to get IComponentHandler COM pointer");
                }

                // The controller is synced from component state via `setComponentState` in
                // `load_state`; transferring it here at load hangs some plugins.
                log::debug!("Skipping component→controller state transfer at load");
            }

            // Editor resize plumbing (an IPlugFrame the view can call into),
            // plus the Linux IRunLoop registry VSTGUI-based editors need.
            let editor_resize = Arc::new(Mutex::new(None));
            #[cfg(target_os = "linux")]
            let run_loop = Arc::new(Mutex::new(RunLoopRegistry::new()));
            #[cfg(target_os = "linux")]
            let plug_frame = create_host_plug_frame(editor_resize.clone(), run_loop.clone());
            #[cfg(not(target_os = "linux"))]
            let plug_frame = create_host_plug_frame(editor_resize.clone());

            // Create event lists
            log::debug!("Step 13: Creating event lists...");
            let input_events = create_event_list();
            let output_events = create_event_list();
            log::debug!("Event lists created");

            // Extract plugin info from the factory and component. Keyed on the class that was
            // actually instantiated, so a multi-class factory reports the uid the host can
            // reload (or respawn under isolation) to get this same component back.
            let info = Self::extract_plugin_info(path, &factory, &component, class_index)?;

            let has_gui = controller.is_some() && {
                if let Some(ref ctrl) = controller {
                    let view_type = c"editor".as_ptr();
                    let view_ptr = ctrl.createView(view_type);
                    if !view_ptr.is_null() {
                        // Release the probe view; never call `removed()` on it — that pairs with
                        // `attached()`, and an unmatched `removed()` crashes some plugins that
                        // initialize their close state only on attach.
                        let _ = ComPtr::<IPlugView>::from_raw(view_ptr);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            let mut updated_info = info;
            updated_info.has_gui = has_gui;

            log::info!(
                "Plugin info: {} by {}",
                updated_info.name,
                updated_info.vendor
            );

            let mut plugin = Self {
                component,
                processor,
                controller,
                single_component,
                info: updated_info,
                is_active: false,
                is_processing: false,
                sample_rate: 44100.0,
                block_size: 512,
                applied_setup: None,
                tempo: 120.0,
                time_sig_numerator: 4,
                time_sig_denominator: 4,
                playing: true,
                process_mode: crate::plugin::ProcessMode::Realtime,
                next_note_id: 1,
                active_notes: Vec::with_capacity(MAX_TRACKED_NOTES),
                process_data: None,
                component_handler: Some(component_handler),
                pending_param_changes: Vec::with_capacity(MAX_PENDING_PARAM_CHANGES),
                dropped_param_changes: 0,
                gui_param_changes_for_host: Arc::new(Mutex::new(Vec::with_capacity(
                    MAX_EDITOR_FEEDBACK,
                ))),
                input_events,
                output_events,
                chunk_events: Vec::with_capacity(MAX_QUEUED_EVENTS),
                output_midi: Arc::new(ArrayQueue::new(MAX_OUTPUT_MIDI)),
                plugin_view: None,
                plug_frame,
                editor_resize,
                #[cfg(target_os = "linux")]
                run_loop,
                _host_app: host_app,
                _module: module,
            };
            // From here on `plugin` owns the teardown: dropping it runs the same ordered
            // sequence the guard would.
            initialized.disarm();

            // VST3 lifecycle: `setupProcessing` and bus activation require an INACTIVE
            // component, and a plugin sizes its DSP buffers when it goes active, from the
            // ProcessSetup it was last given. So set up first, activate second — activating an
            // un-set-up component hands it whatever defaults it was born with.
            //
            // Both are best-effort here, and neither failure fails the load: this runs at the
            // internal default configuration (the host applies its real sample rate and block
            // size immediately after load), the plugin remains fully inspectable either way, and
            // `start_processing` re-runs both with the real settings and reports a genuine
            // failure there.
            if let Err(e) = plugin.setup_processing() {
                log::warn!("setupProcessing failed at load ({e}); deferring to start_processing");
            } else if let Err(e) = plugin.activate() {
                log::warn!(
                    "Component activation failed at load ({e}); deferring to start_processing"
                );
            }

            log::info!("=== PLUGIN LOADING COMPLETE ===");
            log::info!(
                "Has GUI: {}, Active: {}",
                plugin.info.has_gui,
                plugin.is_active
            );
            Ok(plugin)
        }
    }

    /// Put the component into the active state (`IComponent::setActive(true)`).
    ///
    /// Only valid once the component has been given a `ProcessSetup` — see the ordering note in
    /// [`Self::load`].
    fn activate(&mut self) -> Result<()> {
        let result = unsafe { self.component.setActive(1) };
        if result != kResultOk {
            return Err(Error::Other(format!(
                "Failed to activate component: {result:#x}"
            )));
        }
        self.is_active = true;
        Ok(())
    }

    /// Describe the plugin class at `class_index` — the class [`Self::create_component`]
    /// actually instantiated, not merely the first audio class the factory lists. A factory can
    /// export several audio classes (a synth plus its effect sibling, or per-format variants),
    /// and the reported `uid` is what a host stores to reload this exact plugin.
    fn extract_plugin_info(
        path: &std::path::Path,
        factory: &ComPtr<IPluginFactory>,
        component: &ComPtr<IComponent>,
        class_index: i32,
    ) -> Result<PluginInfo> {
        unsafe {
            // Get factory info
            let mut factory_info: PFactoryInfo = std::mem::zeroed();
            factory.getFactoryInfo(&mut factory_info);
            let vendor = crate::internal::utils::c_str_to_string(&factory_info.vendor);

            let mut class_info: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(class_index, &mut class_info) != kResultOk {
                return Err(Error::PluginLoadFailed(format!(
                    "Could not read class info for the instantiated class (index {class_index})"
                )));
            }

            let name = crate::internal::utils::c_str_to_string(&class_info.name);
            let uid = format_class_uid(&class_info.cid);

            // Count audio buses
            let audio_inputs = component.getBusCount(kAudio as i32, kInput as i32) as u32;
            let audio_outputs = component.getBusCount(kAudio as i32, kOutput as i32) as u32;

            // Real version + sub-categories via IPluginFactory2 when the factory
            // provides it; left empty (honest) rather than faked when it doesn't.
            let (version, category) = factory
                .cast::<IPluginFactory2>()
                .and_then(|f2| {
                    let mut info2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(class_index, &mut info2) == kResultOk {
                        Some((
                            crate::internal::utils::c_str_to_string(&info2.version),
                            crate::internal::utils::c_str_to_string(&info2.subCategories),
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            // MIDI capability from the presence of event buses, not a guess.
            let has_midi_input = component.getBusCount(kEvent as i32, kInput as i32) > 0;
            let has_midi_output = component.getBusCount(kEvent as i32, kOutput as i32) > 0;

            Ok(PluginInfo {
                path: path.to_path_buf(),
                name,
                vendor,
                version,
                category,
                uid,
                audio_inputs,
                audio_outputs,
                has_gui: false, // Will be updated by caller
                has_midi_input,
                has_midi_output,
            })
        }
    }

    /// Find and create the audio component from the factory, returning it together with the
    /// factory class index it came from (so the reported [`PluginInfo`] describes that class).
    unsafe fn create_component(
        factory: &ComPtr<IPluginFactory>,
    ) -> Result<(ComPtr<IComponent>, i32)> {
        let num_classes = factory.countClasses();

        for i in 0..num_classes {
            let mut class_info = std::mem::zeroed();
            if factory.getClassInfo(i, &mut class_info) == kResultOk {
                let category = crate::internal::utils::c_str_to_string(&class_info.category);

                if category.contains("Audio Module Class") {
                    let mut component_ptr: *mut IComponent = ptr::null_mut();

                    let result = factory.createInstance(
                        class_info.cid.as_ptr() as *const std::os::raw::c_char,
                        IComponent::IID.as_ptr() as *const std::os::raw::c_char,
                        &mut component_ptr as *mut _ as *mut _,
                    );

                    if result == kResultOk && !component_ptr.is_null() {
                        let component = ComPtr::from_raw(component_ptr).ok_or_else(|| {
                            Error::PluginLoadFailed("Failed to create component".to_string())
                        })?;
                        return Ok((component, i));
                    }
                }
            }
        }

        Err(Error::PluginLoadFailed(
            "No audio component found in plugin".to_string(),
        ))
    }

    /// Map the configured [`ProcessMode`](crate::plugin::ProcessMode) to the VST3 enum value.
    fn vst_process_mode(&self) -> i32 {
        match self.process_mode {
            crate::plugin::ProcessMode::Offline => ProcessModes_::kOffline as i32,
            crate::plugin::ProcessMode::Realtime => ProcessModes_::kRealtime as i32,
        }
    }

    /// The configuration currently baked into the plugin via `setupProcessing`.
    fn current_setup(&self) -> AppliedSetup {
        AppliedSetup {
            sample_rate: self.sample_rate,
            block_size: self.block_size,
            process_mode: self.vst_process_mode(),
        }
    }

    /// Set up processing with the current configuration. VST3 requires the component to be
    /// **inactive**; every caller either runs before the first activation or deactivates first.
    fn setup_processing(&mut self) -> Result<()> {
        unsafe {
            // Set up processing
            let setup = ProcessSetup {
                processMode: self.vst_process_mode(),
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: self.block_size as i32,
                sampleRate: self.sample_rate,
            };

            let result = self.processor.setupProcessing(&setup as *const _ as *mut _);
            if result != kResultOk {
                return Err(Error::InterfaceError(format!(
                    "Failed to setup processing: {:#x}",
                    result
                )));
            }

            // Create process data
            self.create_process_data()?;
            self.applied_setup = Some(self.current_setup());

            Ok(())
        }
    }

    /// Create processing data structures
    // `as u32` on the StatesAndFlags_ constants is required where they are generated as
    // `i32`; on targets where they are already `u32` clippy flags it as redundant.
    #[allow(clippy::unnecessary_cast)]
    fn create_process_data(&mut self) -> Result<()> {
        unsafe {
            let mut data = Box::new(HostProcessData {
                process_data: std::mem::zeroed(),
                input_buffers: Vec::new(),
                output_buffers: Vec::new(),
                input_bus_buffers: Vec::new(),
                output_bus_buffers: Vec::new(),
                process_context: std::mem::zeroed(),
                input_param_changes: ComWrapper::new(ParameterChanges::default()),
                output_param_changes: ComWrapper::new(ParameterChanges::default()),
                input_channel_ptrs: SendChannelPtrs(Vec::new()),
                output_channel_ptrs: SendChannelPtrs(Vec::new()),
            });

            // Initialize process context
            data.process_context.sampleRate = self.sample_rate;
            data.process_context.tempo = self.tempo;
            data.process_context.timeSigNumerator = self.time_sig_numerator;
            data.process_context.timeSigDenominator = self.time_sig_denominator;
            data.process_context.state = process_context_state(self.playing);

            // Set up process data
            data.process_data.processMode = self.vst_process_mode();
            data.process_data.numSamples = self.block_size as i32;
            data.process_data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as i32;
            data.process_data.processContext = &mut data.process_context;

            // Set up event lists
            data.process_data.inputEvents = self
                .input_events
                .as_com_ref::<IEventList>()
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(ptr::null_mut());
            data.process_data.outputEvents = self
                .output_events
                .as_com_ref::<IEventList>()
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(ptr::null_mut());

            // Set up parameter changes
            data.process_data.inputParameterChanges = data
                .input_param_changes
                .as_com_ref::<IParameterChanges>()
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(ptr::null_mut());
            data.process_data.outputParameterChanges = data
                .output_param_changes
                .as_com_ref::<IParameterChanges>()
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(ptr::null_mut());

            // Prepare buffers
            self.prepare_buffers(&mut data)?;

            self.process_data = Some(data);
            Ok(())
        }
    }

    /// Prepare audio buffers based on plugin bus configuration
    unsafe fn prepare_buffers(&mut self, data: &mut HostProcessData) -> Result<()> {
        // Get bus counts
        let input_bus_count = self.component.getBusCount(kAudio as i32, kInput as i32);
        let output_bus_count = self.component.getBusCount(kAudio as i32, kOutput as i32);

        // Clear existing buffers
        data.input_buffers.clear();
        data.output_buffers.clear();
        data.input_bus_buffers.clear();
        data.output_bus_buffers.clear();

        // Prepare input buffers
        for bus_idx in 0..input_bus_count {
            let mut bus_info: BusInfo = std::mem::zeroed();
            if self
                .component
                .getBusInfo(kAudio as i32, kInput as i32, bus_idx, &mut bus_info)
                == kResultOk
            {
                let channel_count = bus_info.channelCount;

                // Activate the bus
                self.component
                    .activateBus(kAudio as i32, kInput as i32, bus_idx, 1);

                // Create buffers for this bus
                for _ in 0..channel_count {
                    let buffer = vec![0.0f32; self.block_size];
                    data.input_buffers.push(buffer);
                }

                // Create AudioBusBuffers struct
                let mut audio_bus_buffer: AudioBusBuffers = std::mem::zeroed();
                audio_bus_buffer.numChannels = channel_count;
                data.input_bus_buffers.push(audio_bus_buffer);
            }
        }

        // Prepare output buffers
        for bus_idx in 0..output_bus_count {
            let mut bus_info: BusInfo = std::mem::zeroed();
            if self
                .component
                .getBusInfo(kAudio as i32, kOutput as i32, bus_idx, &mut bus_info)
                == kResultOk
            {
                let channel_count = bus_info.channelCount;

                // Activate the bus
                self.component
                    .activateBus(kAudio as i32, kOutput as i32, bus_idx, 1);

                // Create buffers for this bus
                for _ in 0..channel_count {
                    let buffer = vec![0.0f32; self.block_size];
                    data.output_buffers.push(buffer);
                }

                // Create AudioBusBuffers struct
                let mut audio_bus_buffer: AudioBusBuffers = std::mem::zeroed();
                audio_bus_buffer.numChannels = channel_count;
                data.output_bus_buffers.push(audio_bus_buffer);
            }
        }

        // Set up process data counts
        data.process_data.numInputs = data.input_bus_buffers.len() as i32;
        data.process_data.numOutputs = data.output_bus_buffers.len() as i32;

        // Build the channel-pointer arrays ONCE and point each bus + the process data at the
        // (now stable) buffers. process() reuses these without allocating per block.
        let build_ptrs = |buffers: &mut [Vec<f32>], buses: &[AudioBusBuffers]| {
            let mut per_bus: Vec<Vec<*mut f32>> = Vec::with_capacity(buses.len());
            let mut chan = 0usize;
            for bus in buses {
                let mut ptrs = Vec::with_capacity(bus.numChannels as usize);
                for _ in 0..bus.numChannels {
                    if chan < buffers.len() {
                        ptrs.push(buffers[chan].as_mut_ptr());
                        chan += 1;
                    }
                }
                per_bus.push(ptrs);
            }
            per_bus
        };
        data.input_channel_ptrs =
            SendChannelPtrs(build_ptrs(&mut data.input_buffers, &data.input_bus_buffers));
        data.output_channel_ptrs = SendChannelPtrs(build_ptrs(
            &mut data.output_buffers,
            &data.output_bus_buffers,
        ));

        for (i, bus) in data.input_bus_buffers.iter_mut().enumerate() {
            if !data.input_channel_ptrs.0[i].is_empty() {
                bus.__field0.channelBuffers32 = data.input_channel_ptrs.0[i].as_mut_ptr();
            }
        }
        for (i, bus) in data.output_bus_buffers.iter_mut().enumerate() {
            if !data.output_channel_ptrs.0[i].is_empty() {
                bus.__field0.channelBuffers32 = data.output_channel_ptrs.0[i].as_mut_ptr();
            }
        }
        data.process_data.inputs = if data.input_bus_buffers.is_empty() {
            ptr::null_mut()
        } else {
            data.input_bus_buffers.as_mut_ptr()
        };
        data.process_data.outputs = if data.output_bus_buffers.is_empty() {
            ptr::null_mut()
        } else {
            data.output_bus_buffers.as_mut_ptr()
        };

        log::debug!(
            "Prepared buffers: {} input buses, {} output buses, {} input channels, {} output channels",
            input_bus_count,
            output_bus_count,
            data.input_buffers.len(),
            data.output_buffers.len()
        );

        Ok(())
    }
}

impl PluginImpl {
    /// Process exactly `frames` samples starting at `frame_offset` within the caller's
    /// buffers. `frames` is always <= the configured `block_size`, which is what the plugin
    /// was set up to accept; `process` splits a larger caller block into successive chunks.
    ///
    /// The plugin-facing event list has already been staged with this chunk's events (see
    /// [`stage_chunk_events`]); queued parameter changes are selected the same way here, by the
    /// chunk their offset falls in. `is_last` marks the chunk that absorbs anything scheduled
    /// past the end of the caller's block.
    fn process_chunk(
        &mut self,
        buffers: &mut AudioBuffers,
        frame_offset: usize,
        frames: usize,
        is_last: bool,
    ) -> Result<()> {
        let result = if let Some(ref mut data) = self.process_data {
            unsafe {
                // Clear output events only - input events should be preserved for processing
                self.output_events.clear();

                // Clear the output parameter queue too. VST3's ProcessData::outputParameterChanges
                // describes changes for the *current* processing block only (see
                // Steinberg::Vst::ProcessData / IParameterChanges docs); the reference
                // ParameterChanges host helper exposes clearQueue() for exactly this reset.
                // Without this, addParameterData()/addPoint() would keep appending to queues
                // from prior blocks, mixing stale points into new ones, growing point storage
                // unbounded, and risking a reallocation on the audio thread long after warm-up.
                data.output_param_changes.clear_all();

                // VST3 allows numSamples to vary up to the maximum given to setupProcessing; the
                // caller's block may be shorter (BufferSize::Default gives variable sizes) or, if
                // it was longer, `process` has already split it into chunks of at most that
                // maximum.
                let frames = frames.min(self.block_size);
                data.process_data.numSamples = frames as i32;

                // Feed the queued parameter changes that belong to this chunk into the
                // processor's input queue, rebased to the chunk — the same routing the events
                // got in `stage_chunk_events`, so automation and MIDI stay aligned across a
                // split block.
                for pc in &self.pending_param_changes {
                    if let Some(off) = chunk_offset(pc.sample_offset, frame_offset, frames, is_last)
                    {
                        data.input_param_changes.enqueue(pc.id, off, pc.value);
                    }
                }

                // Route parameter edits the plugin's *own editor* reported via performEdit into
                // the processor too, so turning a knob in the plugin GUI affects the audio — not
                // just the host's display. (Some plugins relay editor→processor internally over
                // the component/controller connection; others rely on the host to do this. We do
                // it unconditionally; a plugin that also self-relays just gets the same value
                // twice in the same block, which is idempotent.) Drained here at offset 0 and
                // stashed for the host's display poll (get_parameter_changes).
                if let Some(ref handler) = self.component_handler {
                    if let Ok(mut gui_changes) = handler.parameter_changes.lock() {
                        if !gui_changes.is_empty() {
                            for &(id, value) in gui_changes.iter() {
                                data.input_param_changes.enqueue(id, 0, value);
                            }
                            if let Ok(mut stash) = self.gui_param_changes_for_host.lock() {
                                // Bounded: nothing drains the stash unless the host polls
                                // `get_parameter_changes`, and the realtime runner never does, so
                                // an unbounded append here would grow forever and reallocate on
                                // the audio thread. Both buffers are pre-reserved to the cap, so
                                // the steady-state append allocates nothing.
                                let room = MAX_EDITOR_FEEDBACK.saturating_sub(stash.len());
                                if room >= gui_changes.len() {
                                    stash.append(&mut gui_changes);
                                } else {
                                    stash.extend(gui_changes.drain(..room));
                                    gui_changes.clear();
                                }
                            } else {
                                gui_changes.clear();
                            }
                        }
                    }
                }

                // Copy this chunk's input audio to the plugin buffers (length-clamped — never
                // assume the caller's block equals the configured block size).
                for (ch_idx, channel) in buffers.inputs.iter().enumerate() {
                    if ch_idx < data.input_buffers.len() {
                        // Clamp the START as well as the length: the block length comes from the
                        // first channel, but `AudioBuffers`' channel Vecs are public and need not
                        // all be the same length. A short channel would otherwise produce an
                        // out-of-range slice (`ch[start..start]` still panics when start > len).
                        let start = frame_offset.min(channel.len());
                        let n = frames
                            .min(data.input_buffers[ch_idx].len())
                            .min(channel.len() - start);
                        data.input_buffers[ch_idx][..n].copy_from_slice(&channel[start..start + n]);
                    }
                }

                // Clear output buffers
                for buffer in &mut data.output_buffers {
                    buffer.fill(0.0);
                }

                // Channel pointers and process-data input/output pointers were wired once in
                // prepare_buffers (buffer addresses are stable), so there's nothing to rebuild
                // here — keeping the steady-state path allocation-free.

                // Process
                let process_result = self.processor.process(&mut data.process_data);

                // Everything from here to the output copy is per-block cleanup and MUST run even
                // when the plugin reported failure. Returning early instead would leave this
                // block's events and parameter changes queued, so the next block would re-deliver
                // every one of them — re-triggering held notes and growing the queues without
                // bound for as long as the plugin keeps failing.

                // Advance the transport so tempo-synced DSP (LFOs, sync'd delays/arps) sees
                // a moving playhead instead of a frozen time-0. The context describes the
                // block that was just processed; advancing here means the next block starts
                // at the new sample position.
                advance_process_context(&mut data.process_context, frames as i64);

                // Clear the staged input events AFTER processing, so the plugin got to see them
                // and the next chunk starts from an empty list.
                self.input_events.clear();
                // Clear the input parameter queue too, so this block's values don't
                // re-stick on the next block.
                data.input_param_changes.clear_all();

                // Capture any MIDI the plugin emitted this block (arpeggiators, MPE, etc.).
                // Drain the event list in place (no `mem::take`) and push each converted event
                // into the lock-free output queue with `force_push`, which drops the oldest event
                // when full. No lock and no allocation on the audio thread even while MIDI flows.
                if !self.output_events.is_empty() {
                    let out = &self.output_midi;
                    self.output_events.for_each_then_clear(|e| {
                        if let Some(m) = event_to_midi(e) {
                            out.force_push(m);
                        }
                    });
                }

                if process_result != kResultOk {
                    // Leave the caller's buffers as they were (the playback bridges pre-fill them
                    // with silence) rather than copying out whatever the failed call left behind.
                    Err(Error::ProcessFailed(process_result))
                } else {
                    // Copy this chunk's output back into the caller's buffers at the same offset
                    // (length-clamped to the actual frames).
                    for (ch_idx, channel) in buffers.outputs.iter_mut().enumerate() {
                        if ch_idx < data.output_buffers.len() {
                            // Clamp the start too — see the matching note on the input copy.
                            let start = frame_offset.min(channel.len());
                            let n = frames
                                .min(data.output_buffers[ch_idx].len())
                                .min(channel.len() - start);
                            channel[start..start + n]
                                .copy_from_slice(&data.output_buffers[ch_idx][..n]);
                        }
                    }
                    Ok(())
                }
            }
        } else {
            Err(Error::Other("Process data not initialized".to_string()))
        };

        result
    }

    /// Distribute the block's queued input events over its chunks and process each one.
    ///
    /// `total` is the caller's block length, already known non-zero by the caller; the empty
    /// block is handled separately (some plugins use a zero-sample call to flush pending
    /// parameter changes).
    fn process_chunks(&mut self, buffers: &mut AudioBuffers, total: usize) -> Result<()> {
        // `.max(1)`: a zero block size would make every chunk empty and never advance `offset`,
        // spinning forever on the audio thread. `Vst3HostBuilder::build` rejects 0, but
        // `block_size` is plain state and this loop must terminate regardless.
        let step = self.block_size.max(1);
        let mut offset = 0;
        while offset < total {
            let frames = (total - offset).min(step);
            let is_last = offset + frames >= total;
            stage_chunk_events(
                &self.chunk_events,
                &self.input_events,
                offset,
                frames,
                is_last,
            );
            self.process_chunk(buffers, offset, frames, is_last)?;
            offset += frames;
        }
        Ok(())
    }
}

impl PluginInternal for PluginImpl {
    fn set_parameter(&mut self, id: u32, value: f64) -> Result<()> {
        self.set_parameter_at(id, value, 0)
    }

    fn set_parameter_at(&mut self, id: u32, value: f64, sample_offset: i32) -> Result<()> {
        if let Some(ref controller) = self.controller {
            // VST3 requires both halves: tell the controller (for GUI/display/formatting)
            // AND feed the processor's input queue (so the change reaches the audio DSP).
            // The processor side is applied at `sample_offset` in the next process() block.
            let result = unsafe { controller.setParamNormalized(id, value) };
            // Deliberately NOT treated as an error. The SDK's reference `EditController` returns
            // kResultTrue on success and kResultFalse for an id it doesn't own, which makes the
            // code look like a usable success signal — but real plugins don't honour that. Dexed
            // (JUCE) returns kResultFalse for parameter ids 0/1/2 that it then applies correctly,
            // so failing the call here would break parameter automation on shipping plugins. Log
            // it for diagnosis and carry on; `log` skips the formatting entirely when the level is
            // disabled, so this costs nothing on the audio path.
            if result != kResultOk && result != kResultTrue {
                log::debug!(
                    "setParamNormalized({id}) returned {result:#x}; applying anyway \
                     (many plugins report kResultFalse on success)"
                );
            }
            if self.pending_param_changes.len() >= MAX_PENDING_PARAM_CHANGES {
                // Only reachable when nothing is draining the queue (the plugin isn't
                // processing), so the dropped change would have arrived as part of a stale flood
                // anyway. The controller half above still ran, so the plugin's own display
                // stays correct.
                self.dropped_param_changes += 1;
                log::warn!(
                    "dropping parameter change for {id}, queue full at \
                     {MAX_PENDING_PARAM_CHANGES} (is the plugin processing?); \
                     {} dropped so far",
                    self.dropped_param_changes
                );
                return Ok(());
            }
            self.pending_param_changes.push(ParameterChange {
                id,
                value,
                sample_offset,
            });
            Ok(())
        } else {
            Err(Error::InterfaceError("No controller available".to_string()))
        }
    }

    fn set_tempo(&mut self, bpm: f64) -> Result<()> {
        self.update_tempo(bpm);
        Ok(())
    }

    fn set_time_signature(&mut self, numerator: i32, denominator: i32) -> Result<()> {
        self.update_time_signature(numerator, denominator);
        Ok(())
    }

    fn set_playing(&mut self, playing: bool) -> Result<()> {
        self.update_playing(playing);
        Ok(())
    }

    fn get_parameter(&self, id: u32) -> Result<f64> {
        if let Some(ref controller) = self.controller {
            unsafe { Ok(controller.getParamNormalized(id)) }
        } else {
            Err(Error::InterfaceError("No controller available".to_string()))
        }
    }

    fn get_all_parameters(&self) -> Result<Vec<Parameter>> {
        let mut params = Vec::new();

        if let Some(ref controller) = self.controller {
            unsafe {
                let count = controller.getParameterCount();

                for i in 0..count {
                    let mut info: ParameterInfo = std::mem::zeroed();
                    if controller.getParameterInfo(i, &mut info) == kResultOk {
                        let param = Parameter {
                            id: info.id,
                            name: crate::internal::utils::vst_string_to_string(&info.title),
                            value: controller.getParamNormalized(info.id),
                            min: 0.0,
                            max: 1.0,
                            default: info.defaultNormalizedValue,
                            unit: crate::internal::utils::vst_string_to_string(&info.units),
                            step_count: info.stepCount,
                            can_automate: (info.flags
                                & ParameterInfo_::ParameterFlags_::kCanAutomate)
                                != 0,
                            is_read_only: (info.flags
                                & ParameterInfo_::ParameterFlags_::kIsReadOnly)
                                != 0,
                            is_bypass: (info.flags & ParameterInfo_::ParameterFlags_::kIsBypass)
                                != 0,
                            flags: info.flags as u32,
                        };
                        params.push(param);
                    }
                }
            }
        }

        Ok(params)
    }

    fn format_parameter(&self, id: u32, normalized: f64) -> Result<String> {
        if let Some(ref controller) = self.controller {
            unsafe {
                let mut buf: String128 = std::mem::zeroed();
                if controller.getParamStringByValue(id, normalized, &mut buf) == kResultOk {
                    return Ok(crate::internal::utils::vst_string_to_string(&buf));
                }
            }
            Err(Error::InvalidParameter(format!(
                "Plugin could not format parameter {id}"
            )))
        } else {
            Err(Error::InterfaceError("No controller available".to_string()))
        }
    }

    fn process(&mut self, buffers: &mut AudioBuffers) -> Result<()> {
        if !self.is_active || !self.is_processing {
            // Unit variant, not a formatted string: a host that stops processing while its stream
            // keeps running hits this once per block on the audio thread.
            return Err(Error::NotProcessing);
        }

        // Flush denormals to zero for the duration of the plugin's processing, restoring the
        // prior FPU state when this returns. Covers both the simple (mutex) and realtime paths,
        // since both funnel audio through here. Hoisted out of the per-chunk loop below so a
        // split block saves and restores the FPU state once, not once per chunk.
        let _denormal = crate::internal::denormal::DenormalGuard::new();

        // How many frames the caller actually supplied.
        let total = buffers
            .outputs
            .iter()
            .chain(buffers.inputs.iter())
            .map(|c| c.len())
            .next()
            .unwrap_or(self.block_size);

        // The plugin was set up with `maxSamplesPerBlock = self.block_size` and may not be handed
        // more than that in one call — but the device can legitimately deliver more: the cpal
        // backend clamps the requested size *up* into the device's supported range, and
        // `BufferSize::Default` lets the device pick whatever it likes. Feeding only the first
        // `block_size` frames and returning would leave the rest of every buffer as silence (an
        // audible gate at the device's block rate) and advance the plugin's transport at a
        // fraction of real time. So split an oversized block into successive chunks instead.
        //
        // Each queued event and parameter change is routed to the chunk its sample offset falls
        // in, rebased to that chunk: an event at offset 1500 of a 2048-frame block fires in the
        // fourth 512-frame chunk at offset 476, not ~20 ms early at the top of the first one.
        // Offsets past the end of the caller's block are clamped into the final chunk.
        self.input_events.take_into(&mut self.chunk_events);

        let result = if total == 0 {
            // Preserve the historical behaviour of still calling the plugin for an empty block —
            // some plugins use a zero-sample call to flush pending parameter changes.
            stage_chunk_events(&self.chunk_events, &self.input_events, 0, 0, true);
            self.process_chunk(buffers, 0, 0, true)
        } else {
            self.process_chunks(buffers, total)
        };

        // Both queues are consumed by this block, on the error path too: leaving them filled
        // would re-deliver every event and parameter change next block — re-triggering held
        // notes and growing without bound for as long as the plugin keeps failing.
        self.chunk_events.clear();
        self.pending_param_changes.clear();
        result
    }

    fn reconfigure(&mut self, sample_rate: f64, block_size: usize) -> Result<()> {
        if self.is_processing {
            return Err(Error::Other(
                "cannot reconfigure while processing".to_string(),
            ));
        }
        unsafe {
            // VST3 requires the component to be inactive when setupProcessing is called.
            let was_active = self.is_active;
            if was_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            let (old_sr, old_bs) = (self.sample_rate, self.block_size);
            self.sample_rate = sample_rate;
            self.block_size = block_size;

            // Re-run setupProcessing and rebuild process data / buffers for the new size. On
            // failure restore the cached config so it stays consistent with the still-current
            // (previous) process_data — `process_data` is only swapped in on success. The
            // component is left inactive; `start_processing` reactivates and re-runs setup.
            if let Err(e) = self.setup_processing() {
                self.sample_rate = old_sr;
                self.block_size = old_bs;
                return Err(e);
            }

            if was_active {
                let result = self.component.setActive(1);
                if result != kResultOk {
                    return Err(Error::Other(format!(
                        "Failed to reactivate after reconfigure: {:#x}",
                        result
                    )));
                }
                self.is_active = true;
            }
        }
        Ok(())
    }

    fn set_process_mode(&mut self, mode: crate::plugin::ProcessMode) -> Result<()> {
        if self.is_processing {
            return Err(Error::Other(
                "cannot set process mode while processing".to_string(),
            ));
        }
        unsafe {
            // VST3 requires the component inactive for setupProcessing; mirror reconfigure.
            let was_active = self.is_active;
            if was_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            // Store the mode first so setup_processing bakes it into BOTH ProcessSetup and
            // the freshly rebuilt process_data; restore it if setup fails so the cached mode
            // always reflects the last successfully-applied configuration.
            let old_mode = self.process_mode;
            self.process_mode = mode;
            if let Err(e) = self.setup_processing() {
                self.process_mode = old_mode;
                return Err(e);
            }

            if was_active {
                let result = self.component.setActive(1);
                if result != kResultOk {
                    return Err(Error::Other(format!(
                        "Failed to reactivate after set_process_mode: {:#x}",
                        result
                    )));
                }
                self.is_active = true;
            }
        }
        Ok(())
    }

    fn bus_arrangements(&self) -> Result<crate::audio::BusArrangements> {
        use crate::audio::SpeakerArrangement;
        unsafe {
            let read = |dir: i32| -> Vec<SpeakerArrangement> {
                let count = self.component.getBusCount(kAudio as i32, dir);
                (0..count)
                    .map(|idx| {
                        let mut arr: u64 = 0;
                        self.processor.getBusArrangement(dir, idx, &mut arr);
                        SpeakerArrangement::from_raw(arr)
                    })
                    .collect()
            };
            Ok(crate::audio::BusArrangements {
                inputs: read(kInput as i32),
                outputs: read(kOutput as i32),
            })
        }
    }

    fn set_bus_arrangements(
        &mut self,
        inputs: &[crate::audio::SpeakerArrangement],
        outputs: &[crate::audio::SpeakerArrangement],
    ) -> Result<()> {
        if self.is_processing {
            return Err(Error::Other(
                "cannot set bus arrangements while processing".to_string(),
            ));
        }
        let mut in_raw: Vec<u64> = inputs.iter().map(|a| a.raw()).collect();
        let mut out_raw: Vec<u64> = outputs.iter().map(|a| a.raw()).collect();
        unsafe {
            // VST3 requires the component inactive for setBusArrangements + setupProcessing.
            let was_active = self.is_active;
            if was_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            // A plugin MAY return kResultFalse to mean "kept my own layout" — not an error.
            // Either way, re-running setup rebuilds buffers from the arrangement it actually has.
            self.processor.setBusArrangements(
                in_raw.as_mut_ptr(),
                in_raw.len() as i32,
                out_raw.as_mut_ptr(),
                out_raw.len() as i32,
            );

            self.setup_processing()?;

            if was_active {
                let result = self.component.setActive(1);
                if result != kResultOk {
                    return Err(Error::Other(format!(
                        "Failed to reactivate after set_bus_arrangements: {:#x}",
                        result
                    )));
                }
                self.is_active = true;
            }
        }
        Ok(())
    }

    fn set_bus_active(
        &mut self,
        media_type: crate::audio::MediaType,
        direction: crate::audio::BusDirection,
        bus_index: i32,
        active: bool,
    ) -> Result<()> {
        use crate::audio::{BusDirection, MediaType};
        if self.is_processing {
            return Err(Error::Other(
                "cannot activate a bus while processing".to_string(),
            ));
        }
        // VST3 bus activation requires the component inactive (it's a setup-time operation).
        let media = match media_type {
            MediaType::Audio => kAudio as i32,
            MediaType::Event => kEvent as i32,
        };
        let dir = match direction {
            BusDirection::Input => kInput as i32,
            BusDirection::Output => kOutput as i32,
        };
        unsafe {
            let count = self.component.getBusCount(media, dir);
            if bus_index < 0 || bus_index >= count {
                return Err(Error::InvalidParameter(format!(
                    "bus index {bus_index} out of range for {media_type:?} {direction:?} \
                     bus (count {count})"
                )));
            }

            let was_active = self.is_active;
            if was_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            let result = self
                .component
                .activateBus(media, dir, bus_index, active as u8);

            if was_active {
                let reactivate = self.component.setActive(1);
                if reactivate != kResultOk {
                    return Err(Error::Other(format!(
                        "Failed to reactivate after set_bus_active: {reactivate:#x}"
                    )));
                }
                self.is_active = true;
            }

            if result != kResultOk {
                return Err(Error::Other(format!(
                    "activateBus failed for {media_type:?} {direction:?} bus {bus_index}: \
                     {result:#x}"
                )));
            }
        }
        Ok(())
    }

    fn send_midi_event(&mut self, event: MidiEvent) -> Result<()> {
        self.send_midi_event_at(event, 0)
    }

    fn send_midi_event_at(&mut self, event: MidiEvent, sample_offset: i32) -> Result<()> {
        // Floor to non-negative (the VST3 SDK treats a negative sampleOffset as undefined);
        // `process()` additionally clamps queued offsets to the actual block length.
        let sample_offset = sample_offset.max(0);
        unsafe {
            let mut vst_event: Event = std::mem::zeroed();
            vst_event.busIndex = 0;
            vst_event.sampleOffset = sample_offset;
            vst_event.ppqPosition = 0.0;
            vst_event.flags = Event_::EventFlags_::kIsLive as u16;

            match event {
                MidiEvent::NoteOn {
                    channel,
                    note,
                    velocity,
                } => {
                    vst_event.r#type = kNoteOnEvent as u16;
                    vst_event.__field0.noteOn.channel = channel.as_index() as i16;
                    vst_event.__field0.noteOn.pitch = note as i16;
                    vst_event.__field0.noteOn.tuning = 0.0;
                    vst_event.__field0.noteOn.velocity = velocity as f32 / 127.0;
                    vst_event.__field0.noteOn.length = 0;
                    vst_event.__field0.noteOn.noteId = -1;
                }
                MidiEvent::NoteOff {
                    channel,
                    note,
                    velocity,
                } => {
                    vst_event.r#type = kNoteOffEvent as u16;
                    vst_event.__field0.noteOff.channel = channel.as_index() as i16;
                    vst_event.__field0.noteOff.pitch = note as i16;
                    vst_event.__field0.noteOff.velocity = velocity as f32 / 127.0;
                    vst_event.__field0.noteOff.noteId = -1;
                    vst_event.__field0.noteOff.tuning = 0.0;
                }
                MidiEvent::ControlChange {
                    channel,
                    controller,
                    value,
                } => {
                    // Control changes are sent as legacy MIDI CC events.
                    return self.send_legacy_midi_cc(channel, controller, value, sample_offset);
                }
                MidiEvent::PitchBend { channel, value } => {
                    // 14-bit pitch bend (0..=16383) carried as two MIDI data bytes via
                    // the legacy controller channel kPitchBend (129): value = LSB, value2 = MSB.
                    vst_event.r#type = kLegacyMIDICCOutEvent as u16;
                    vst_event.__field0.midiCCOut.controlNumber =
                        ControllerNumbers_::kPitchBend as u8;
                    vst_event.__field0.midiCCOut.channel =
                        channel.as_index() as std::os::raw::c_char;
                    vst_event.__field0.midiCCOut.value = (value & 0x7F) as std::os::raw::c_char;
                    vst_event.__field0.midiCCOut.value2 =
                        ((value >> 7) & 0x7F) as std::os::raw::c_char;
                }
                MidiEvent::ChannelAftertouch { channel, pressure } => {
                    // Channel pressure via legacy controller channel kAfterTouch (128).
                    vst_event.r#type = kLegacyMIDICCOutEvent as u16;
                    vst_event.__field0.midiCCOut.controlNumber =
                        ControllerNumbers_::kAfterTouch as u8;
                    vst_event.__field0.midiCCOut.channel =
                        channel.as_index() as std::os::raw::c_char;
                    vst_event.__field0.midiCCOut.value = (pressure & 0x7F) as std::os::raw::c_char;
                    vst_event.__field0.midiCCOut.value2 = 0;
                }
                MidiEvent::PolyAftertouch {
                    channel,
                    note,
                    pressure,
                } => {
                    // Per-note pressure maps to a first-class VST3 poly-pressure event.
                    vst_event.r#type = kPolyPressureEvent as u16;
                    vst_event.__field0.polyPressure.channel = channel.as_index() as i16;
                    vst_event.__field0.polyPressure.pitch = note as i16;
                    vst_event.__field0.polyPressure.pressure = pressure as f32 / 127.0;
                    vst_event.__field0.polyPressure.noteId = -1;
                }
                MidiEvent::ProgramChange { program, .. } => {
                    // VST3 has no MIDI program-change event; a program change is routed to the
                    // root unit's (id 0) program-change parameter via IUnitInfo. The MIDI
                    // channel does not map cleanly to a VST3 unit, so we target the root unit
                    // regardless of channel. select_program drives the controller + processor
                    // queue itself, so we return directly rather than falling through to the
                    // event-list add below.
                    return self.select_program(0, program as i32);
                }
            }

            self.input_events.add_event(vst_event);
        }
        Ok(())
    }

    fn start_processing(&mut self) -> Result<()> {
        unsafe {
            // The configuration may have moved since the last setup (`set_audio_config` after
            // load, or a device change), and a plugin sizes its DSP buffers when it goes active,
            // from the ProcessSetup it was last given. So apply a changed configuration the way
            // `reconfigure` does — deactivate, set up, reactivate — rather than calling
            // setupProcessing on a running component, which VST3 forbids. An unchanged
            // configuration needs no setup at all: the plugin still has it.
            if self.applied_setup != Some(self.current_setup()) {
                if self.is_active {
                    self.component.setActive(0);
                    self.is_active = false;
                }
                self.setup_processing()?;
            }

            if !self.is_active {
                self.activate()?;
            }

            // Start processing. `setProcessing` is an optional notification — a plugin
            // may return kNotImplemented (e.g. u-he), which is not an error: it simply
            // doesn't need the start/stop signal and still processes audio normally.
            let result = self.processor.setProcessing(1);
            if result != kResultOk && result != kNotImplemented {
                return Err(Error::Other(format!(
                    "Failed to start processing: {:#x}",
                    result
                )));
            }

            self.is_processing = true;
            log::debug!("Plugin processing started successfully");
            Ok(())
        }
    }

    fn stop_processing(&mut self) -> Result<()> {
        unsafe {
            if self.is_processing {
                self.processor.setProcessing(0);
                self.is_processing = false;
            }

            if self.is_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            // Nothing can still be sounding, so no note-on is outstanding.
            self.active_notes.clear();

            Ok(())
        }
    }

    fn has_editor(&self) -> bool {
        // First check our cached value
        if self.info.has_gui {
            return true;
        }

        // Otherwise do a runtime check
        if let Some(ref controller) = self.controller {
            unsafe {
                // Check if controller can create an editor view
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if !view_ptr.is_null() {
                    // Release the probe view; never call `removed()` on it — that pairs with
                    // `attached()`, and an unmatched `removed()` crashes some plugins that
                    // initialize their close state only on attach.
                    let _ = ComPtr::<IPlugView>::from_raw(view_ptr);
                    true
                } else {
                    false
                }
            }
        } else {
            false
        }
    }

    fn open_editor(&mut self, parent: *mut std::ffi::c_void) -> Result<()> {
        if self.plugin_view.is_some() {
            return Err(Error::Other("Editor already open".to_string()));
        }

        if let Some(ref controller) = self.controller {
            unsafe {
                // Create editor view
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if view_ptr.is_null() {
                    return Err(Error::Other("Failed to create editor view".to_string()));
                }

                let view = ComPtr::<IPlugView>::from_raw(view_ptr)
                    .ok_or_else(|| Error::Other("Failed to wrap view".to_string()))?;

                // Get view size
                let mut view_rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: 400,
                    bottom: 300,
                };

                if view.getSize(&mut view_rect) != kResultOk {
                    return Err(Error::Other("Failed to get view size".to_string()));
                }

                // Hand the plugin an IPlugFrame (before attach, per the SDK) so it can
                // request host-side resizes; requests land in `editor_resize`.
                if let Some(frame) = self.plug_frame.to_com_ptr::<IPlugFrame>() {
                    view.setFrame(frame.as_ptr());
                }

                // Platform-specific attachment
                #[cfg(target_os = "macos")]
                let platform_type = c"NSView".as_ptr();
                #[cfg(target_os = "windows")]
                let platform_type = c"HWND".as_ptr();
                #[cfg(target_os = "linux")]
                let platform_type = c"X11EmbedWindowID".as_ptr();
                #[cfg(target_os = "android")]
                let platform_type = c"kNativeWindowHandle".as_ptr();

                // Check platform support
                if view.isPlatformTypeSupported(platform_type) != kResultOk {
                    return Err(Error::Other("Platform type not supported".to_string()));
                }

                // Attach to parent window
                let attach_result = view.attached(parent, platform_type);
                if attach_result != kResultOk {
                    return Err(Error::Other(format!(
                        "Failed to attach view: {:#x}",
                        attach_result
                    )));
                }

                self.plugin_view = Some(view);
                Ok(())
            }
        } else {
            Err(Error::Other("No controller available".to_string()))
        }
    }

    fn close_editor(&mut self) -> Result<()> {
        if let Some(view) = self.plugin_view.take() {
            unsafe {
                view.removed();
            }
        }
        // Drop any run-loop registrations the editor left behind. A well-behaved plugin
        // unregisters its own handlers and timers during `removed()`, but one that doesn't would
        // otherwise leave live ComPtrs into a view that no longer exists — and since
        // `Plugin::service_run_loop` is public with no editor-open guard, a host driving it from
        // its frame loop would then dispatch straight into the removed view.
        #[cfg(target_os = "linux")]
        if let Ok(mut reg) = self.run_loop.lock() {
            reg.handlers.clear();
            reg.timers.clear();
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn service_run_loop(&mut self) {
        use vst3::Steinberg::Linux::{IEventHandlerTrait, ITimerHandlerTrait};

        // Fire due timers. Snapshot the handlers, then invoke with the lock
        // RELEASED: a callback may re-enter registerTimer/unregisterTimer
        // (VSTGUI does), which takes the same lock.
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        if let Ok(mut reg) = self.run_loop.lock() {
            for timer in reg.timers.iter_mut() {
                if now >= timer.due {
                    timer.due = now + std::time::Duration::from_millis(timer.interval_ms);
                    due.push(timer.handler.clone());
                }
            }
        }
        for handler in due {
            unsafe { handler.onTimer() };
        }

        // Poll registered fds (zero timeout - never blocks the UI thread)
        // and notify ready ones. Same snapshot-then-invoke pattern.
        let handlers: Vec<_> = match self.run_loop.lock() {
            Ok(reg) => reg.handlers.clone(),
            Err(_) => return,
        };
        if handlers.is_empty() {
            return;
        }
        let mut fds: Vec<libc::pollfd> = handlers
            .iter()
            .map(|&(_, fd)| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 0) };
        if ready > 0 {
            for (pfd, (handler, fd)) in fds.iter().zip(handlers.iter()) {
                if pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                    unsafe { handler.onFDIsSet(*fd) };
                }
            }
        }
    }

    fn get_editor_size(&self) -> Result<(i32, i32)> {
        if let Some(ref controller) = self.controller {
            unsafe {
                // Create a temporary view to get size
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if view_ptr.is_null() {
                    return Err(Error::Other(
                        "Failed to create view for size query".to_string(),
                    ));
                }

                let view = ComPtr::<IPlugView>::from_raw(view_ptr)
                    .ok_or_else(|| Error::Other("Failed to wrap plug view".to_string()))?;

                // Get view size
                let mut view_rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: 400,
                    bottom: 300,
                };

                let result = view.getSize(&mut view_rect);

                // Released when `view` drops; do not call `removed()` — it pairs with
                // `attached()`, which this probe never calls.

                if result == kResultOk {
                    let width = view_rect.right - view_rect.left;
                    let height = view_rect.bottom - view_rect.top;
                    Ok((width, height))
                } else {
                    Ok((800, 600)) // Default size
                }
            }
        } else {
            Err(Error::Other("No controller available".to_string()))
        }
    }

    fn get_parameter_changes(&self) -> Vec<(u32, f64)> {
        self.get_parameter_changes()
    }

    fn take_parameter_edits(&mut self) -> Vec<crate::plugin::ParameterEdit> {
        self.component_handler
            .as_ref()
            .map(|h| h.take_parameter_edits())
            .unwrap_or_default()
    }

    fn take_restart_flags(&mut self) -> crate::plugin::RestartFlags {
        self.component_handler
            .as_ref()
            .map(|h| h.take_restart_flags())
            .unwrap_or_default()
    }

    fn take_output_events(&self) -> Vec<MidiEvent> {
        let mut out = Vec::new();
        while let Some(event) = self.output_midi.pop() {
            out.push(event);
        }
        out
    }

    fn output_midi_handle(&self) -> Option<crate::plugin::OutputMidiConsumer> {
        Some(crate::plugin::OutputMidiConsumer::from_queue(
            self.output_midi.clone(),
        ))
    }

    fn take_editor_resize_request(&self) -> Option<(i32, i32)> {
        self.editor_resize.lock().ok().and_then(|mut s| s.take())
    }

    fn latency_samples(&self) -> u32 {
        unsafe { self.processor.getLatencySamples() }
    }

    fn tail_samples(&self) -> u32 {
        unsafe { self.processor.getTailSamples() }
    }

    fn midi_cc_to_parameter(&self, bus: i32, channel: i16, cc: u16) -> Option<u32> {
        let controller = self.controller.as_ref()?;
        let mapping = controller.cast::<IMidiMapping>()?;
        unsafe {
            let mut id: ParamID = 0;
            let result =
                mapping.getMidiControllerAssignment(bus, channel, cc as CtrlNumber, &mut id);
            // kResultTrue == kResultOk: a mapping exists and `id` was written.
            (result == kResultOk).then_some(id)
        }
    }

    fn note_on(
        &mut self,
        channel: MidiChannel,
        note: u8,
        velocity: u8,
        sample_offset: i32,
    ) -> Result<crate::midi::NoteId> {
        let id = self.next_note_id;
        self.next_note_id = self.next_note_id.wrapping_add(1).max(1);
        // Remember which (channel, pitch) this id stands for. VST3 note-off carries both the
        // noteId *and* the pitch/channel, and plugins that don't track note ids (most non-MPE
        // synths) match the release by pitch — so `note_off` has to reproduce them.
        if self.active_notes.len() >= MAX_TRACKED_NOTES {
            // Full only if a caller started notes it never released (a MIDI panic sends CCs, not
            // note-offs, so entries can also be left behind that way). Evict one in O(1) rather
            // than refusing to track, so new notes keep getting correct releases.
            self.active_notes.swap_remove(0);
        }
        self.active_notes
            .push((id, channel.as_index() as i16, note as i16));
        unsafe {
            let mut ev: Event = std::mem::zeroed();
            ev.busIndex = 0;
            ev.sampleOffset = sample_offset.max(0);
            ev.flags = Event_::EventFlags_::kIsLive as u16;
            ev.r#type = kNoteOnEvent as u16;
            ev.__field0.noteOn.channel = channel.as_index() as i16;
            ev.__field0.noteOn.pitch = note as i16;
            ev.__field0.noteOn.velocity = velocity as f32 / 127.0;
            ev.__field0.noteOn.noteId = id;
            self.input_events.add_event(ev);
        }
        Ok(crate::midi::NoteId(id))
    }

    fn note_off(&mut self, id: crate::midi::NoteId, sample_offset: i32) -> Result<()> {
        // Recover the note's channel and pitch from the note-on. Without them the event carries
        // pitch 0 on channel 1, which any synth matching releases by pitch ignores — leaving the
        // real note sounding forever.
        let tracked = self
            .active_notes
            .iter()
            .position(|&(tracked_id, _, _)| tracked_id == id.0)
            .map(|i| self.active_notes.swap_remove(i));
        unsafe {
            let mut ev: Event = std::mem::zeroed();
            ev.busIndex = 0;
            ev.sampleOffset = sample_offset.max(0);
            ev.flags = Event_::EventFlags_::kIsLive as u16;
            ev.r#type = kNoteOffEvent as u16;
            ev.__field0.noteOff.noteId = id.0;
            if let Some((_, channel, pitch)) = tracked {
                ev.__field0.noteOff.channel = channel;
                ev.__field0.noteOff.pitch = pitch;
            }
            // A release velocity of 0 is the SDK's own default for "unspecified".
            self.input_events.add_event(ev);
        }
        Ok(())
    }

    fn send_note_expression(
        &mut self,
        id: crate::midi::NoteId,
        kind: crate::midi::NoteExpressionType,
        value: f64,
        sample_offset: i32,
    ) -> Result<()> {
        unsafe {
            let mut ev: Event = std::mem::zeroed();
            ev.busIndex = 0;
            ev.sampleOffset = sample_offset.max(0);
            ev.flags = Event_::EventFlags_::kIsLive as u16;
            ev.r#type = kNoteExpressionValueEvent as u16;
            ev.__field0.noteExpressionValue.typeId = kind.type_id();
            ev.__field0.noteExpressionValue.noteId = id.0;
            ev.__field0.noteExpressionValue.value = value.clamp(0.0, 1.0);
            self.input_events.add_event(ev);
        }
        Ok(())
    }

    fn note_expressions(
        &self,
        bus: i32,
        channel: i16,
    ) -> Result<Vec<crate::midi::NoteExpressionInfo>> {
        use crate::midi::{NoteExpressionInfo, NoteExpressionType};
        let Some(ctrl) = self
            .controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionController>())
        else {
            return Ok(Vec::new());
        };
        unsafe {
            let count = ctrl.getNoteExpressionCount(bus, channel);
            let mut out = Vec::with_capacity(count.max(0) as usize);
            for i in 0..count {
                let mut info: NoteExpressionTypeInfo = std::mem::zeroed();
                if ctrl.getNoteExpressionInfo(bus, channel, i, &mut info) == kResultOk {
                    use NoteExpressionTypeInfo_::NoteExpressionTypeFlags_ as Flags;
                    let flags = info.flags;
                    out.push(NoteExpressionInfo {
                        kind: NoteExpressionType::from_type_id(info.typeId),
                        title: crate::internal::utils::vst_string_to_string(&info.title),
                        short_title: crate::internal::utils::vst_string_to_string(&info.shortTitle),
                        units: crate::internal::utils::vst_string_to_string(&info.units),
                        default_value: info.valueDesc.defaultValue,
                        min: info.valueDesc.minimum,
                        max: info.valueDesc.maximum,
                        step_count: info.valueDesc.stepCount,
                        is_bipolar: flags & Flags::kIsBipolar as i32 != 0,
                        is_one_shot: flags & Flags::kIsOneShot as i32 != 0,
                        is_absolute: flags & Flags::kIsAbsolute as i32 != 0,
                    });
                }
            }
            Ok(out)
        }
    }

    fn get_units(&self) -> Result<Vec<crate::plugin::PluginUnit>> {
        use crate::plugin::PluginUnit;
        let Some(ref controller) = self.controller else {
            return Ok(Vec::new());
        };
        // IUnitInfo is optional; plugins without it (no units/program lists) return empty.
        let Some(unit_info) = controller.cast::<IUnitInfo>() else {
            return Ok(Vec::new());
        };
        unsafe {
            // First resolve program lists (id -> program names), then attach to units.
            let mut lists: std::collections::HashMap<i32, Vec<String>> =
                std::collections::HashMap::new();
            let list_count = unit_info.getProgramListCount();
            for i in 0..list_count {
                let mut pl: ProgramListInfo = std::mem::zeroed();
                if unit_info.getProgramListInfo(i, &mut pl) != kResultOk {
                    continue;
                }
                let mut programs = Vec::with_capacity(pl.programCount.max(0) as usize);
                for p in 0..pl.programCount {
                    let mut name: String128 = std::mem::zeroed();
                    let s = if unit_info.getProgramName(pl.id, p, &mut name) == kResultOk {
                        crate::internal::utils::vst_string_to_string(&name)
                    } else {
                        String::new()
                    };
                    programs.push(s);
                }
                lists.insert(pl.id, programs);
            }

            let unit_count = unit_info.getUnitCount();
            let mut units = Vec::with_capacity(unit_count.max(0) as usize);
            for i in 0..unit_count {
                let mut ui: UnitInfo = std::mem::zeroed();
                if unit_info.getUnitInfo(i, &mut ui) != kResultOk {
                    continue;
                }
                let programs = lists.get(&ui.programListId).cloned().unwrap_or_default();
                units.push(PluginUnit {
                    id: ui.id,
                    parent_id: ui.parentUnitId,
                    name: crate::internal::utils::vst_string_to_string(&ui.name),
                    programs,
                });
            }
            Ok(units)
        }
    }

    fn select_program(&mut self, unit_id: i32, program_index: i32) -> Result<()> {
        let (param_id, program_count) = self.resolve_program_change(unit_id)?;
        if program_index < 0 || program_index >= program_count {
            return Err(Error::InvalidParameter(format!(
                "program index {program_index} out of range for unit {unit_id} \
                 ({program_count} programs)"
            )));
        }
        // VST3 maps a discrete program list onto the program-change parameter's normalized
        // 0..1 range: index N of a count-C list is N / (C-1). A single-program list maps to 0.
        let normalized = if program_count > 1 {
            program_index as f64 / (program_count - 1) as f64
        } else {
            0.0
        };
        // Drive both halves (controller for display, processor queue for the DSP), exactly as
        // set_parameter does — a program change is just a parameter change in VST3.
        self.set_parameter_at(param_id, normalized, 0)
    }

    fn output_channel_count(&self) -> usize {
        unsafe {
            let bus_count = self.component.getBusCount(kAudio as i32, kOutput as i32);
            let mut total = 0usize;
            for i in 0..bus_count {
                let mut info: BusInfo = std::mem::zeroed();
                if self
                    .component
                    .getBusInfo(kAudio as i32, kOutput as i32, i, &mut info)
                    == kResultOk
                {
                    total += info.channelCount.max(0) as usize;
                }
            }
            total
        }
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        unsafe {
            // Ask the processor (component) to serialize its state into our stream.
            let stream = create_memory_stream();
            let stream_ptr = stream
                .to_com_ptr::<IBStream>()
                .ok_or_else(|| Error::InterfaceError("Failed to create state stream".into()))?;
            let result = self.component.getState(stream_ptr.as_ptr());
            if result != kResultOk {
                return Err(Error::Other(format!(
                    "Plugin does not provide state (getState: {result:#x})"
                )));
            }
            Ok(stream.to_vec())
        }
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        unsafe {
            // Restore the processor state.
            let comp_stream = create_memory_stream_from(data.to_vec());
            let comp_ptr = comp_stream
                .to_com_ptr::<IBStream>()
                .ok_or_else(|| Error::InterfaceError("Failed to create state stream".into()))?;
            let result = self.component.setState(comp_ptr.as_ptr());
            // kNotImplemented is acceptable (some plugins keep all state on the controller).
            if result != kResultOk && result != kNotImplemented {
                return Err(Error::Other(format!(
                    "Failed to restore plugin state (setState: {result:#x})"
                )));
            }

            // For a *separate* controller, push the same bytes so its parameter cache /
            // editor reflect the restored state. A fresh stream is used because setState
            // consumed the first one's cursor. Skipped for single-component plugins, where
            // setState already restored the one shared object (see `single_component`).
            if !self.single_component {
                if let Some(ref controller) = self.controller {
                    let ctrl_stream = create_memory_stream_from(data.to_vec());
                    if let Some(ctrl_ptr) = ctrl_stream.to_com_ptr::<IBStream>() {
                        let r = controller.setComponentState(ctrl_ptr.as_ptr());
                        if r != kResultOk && r != kNotImplemented {
                            log::debug!("Controller setComponentState returned {r:#x} (ignored)");
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// Convert a raw VST3 `Event` (as a plugin emits into its output event list) into a safe
/// [`MidiEvent`]. Returns `None` for event types this library doesn't model.
#[allow(non_upper_case_globals)]
// kNoteOnEvent etc. are VST3 SDK constants
// SDK enum constants (event types, controller numbers) are generated as `i32` on some
// targets (Windows) and `u32` on others (macOS); the `u8` field casts are likewise needed
// where `c_char` is `i8`. We match the `u32` scrutinee against `<const> as u32` and allow
// the cast clippy flags as redundant on the targets where it already matches.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn event_to_midi(e: &Event) -> Option<MidiEvent> {
    unsafe {
        match e.r#type as u32 {
            t if t == kNoteOnEvent as u32 => {
                let n = &e.__field0.noteOn;
                Some(MidiEvent::NoteOn {
                    channel: MidiChannel::from_index(n.channel as u8)?,
                    note: (n.pitch.clamp(0, 127)) as u8,
                    velocity: (n.velocity * 127.0).round().clamp(0.0, 127.0) as u8,
                })
            }
            t if t == kNoteOffEvent as u32 => {
                let n = &e.__field0.noteOff;
                Some(MidiEvent::NoteOff {
                    channel: MidiChannel::from_index(n.channel as u8)?,
                    note: (n.pitch.clamp(0, 127)) as u8,
                    velocity: (n.velocity * 127.0).round().clamp(0.0, 127.0) as u8,
                })
            }
            t if t == kPolyPressureEvent as u32 => {
                let p = &e.__field0.polyPressure;
                Some(MidiEvent::PolyAftertouch {
                    channel: MidiChannel::from_index(p.channel as u8)?,
                    note: (p.pitch.clamp(0, 127)) as u8,
                    pressure: (p.pressure * 127.0).round().clamp(0.0, 127.0) as u8,
                })
            }
            t if t == kLegacyMIDICCOutEvent as u32 => {
                let c = &e.__field0.midiCCOut;
                let channel = MidiChannel::from_index(c.channel as u8)?;
                let value = (c.value as u8) & 0x7F;
                match c.controlNumber as u32 {
                    n if n == ControllerNumbers_::kPitchBend as u32 => Some(MidiEvent::PitchBend {
                        channel,
                        value: (((c.value2 as u16) & 0x7F) << 7) | value as u16,
                    }),
                    n if n == ControllerNumbers_::kAfterTouch as u32 => {
                        Some(MidiEvent::ChannelAftertouch {
                            channel,
                            pressure: value,
                        })
                    }
                    cc if cc < 128 => Some(MidiEvent::ControlChange {
                        channel,
                        controller: cc as u8,
                        value,
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

impl PluginImpl {
    /// Resolve a unit's program-change parameter for [`PluginInternal::select_program`].
    ///
    /// Returns `(param_id, program_count)`: the id of the controller parameter that switches
    /// programs for `unit_id` (the parameter tied to that unit carrying the VST3
    /// `kIsProgramChange` flag), and the number of programs in the unit's program list. Errors
    /// if the plugin has no `IUnitInfo`, the unit is unknown, it has no (non-empty) program
    /// list, or no program-change parameter is found for it.
    fn resolve_program_change(&self, unit_id: i32) -> Result<(u32, i32)> {
        let controller = self
            .controller
            .as_ref()
            .ok_or_else(|| Error::InterfaceError("No controller available".to_string()))?;
        let unit_info = controller.cast::<IUnitInfo>().ok_or_else(|| {
            Error::Other("Plugin does not implement IUnitInfo (no program lists)".to_string())
        })?;
        unsafe {
            // Find the unit and its program list id.
            let unit_count = unit_info.getUnitCount();
            let mut program_list_id = None;
            for i in 0..unit_count {
                let mut ui: UnitInfo = std::mem::zeroed();
                if unit_info.getUnitInfo(i, &mut ui) == kResultOk && ui.id == unit_id {
                    program_list_id = Some(ui.programListId);
                    break;
                }
            }
            let Some(program_list_id) = program_list_id else {
                return Err(Error::InvalidParameter(format!(
                    "unknown unit id {unit_id}"
                )));
            };

            // Look up the program count for that list.
            let list_count = unit_info.getProgramListCount();
            let mut program_count = None;
            for i in 0..list_count {
                let mut pl: ProgramListInfo = std::mem::zeroed();
                if unit_info.getProgramListInfo(i, &mut pl) == kResultOk && pl.id == program_list_id
                {
                    program_count = Some(pl.programCount);
                    break;
                }
            }
            let program_count = match program_count {
                Some(c) if c > 0 => c,
                _ => {
                    return Err(Error::InvalidParameter(format!(
                        "unit {unit_id} has no program list"
                    )))
                }
            };

            // The program-change parameter is the controller parameter tied to this unit
            // (matching unitId) carrying the kIsProgramChange flag.
            let count = controller.getParameterCount();
            for i in 0..count {
                let mut info: ParameterInfo = std::mem::zeroed();
                if controller.getParameterInfo(i, &mut info) != kResultOk {
                    continue;
                }
                let is_program_change =
                    (info.flags & ParameterInfo_::ParameterFlags_::kIsProgramChange) != 0;
                if is_program_change && info.unitId == unit_id {
                    return Ok((info.id, program_count));
                }
            }
            Err(Error::Other(format!(
                "unit {unit_id} has a program list but no program-change parameter"
            )))
        }
    }

    /// Send a legacy MIDI CC event
    fn send_legacy_midi_cc(
        &mut self,
        channel: MidiChannel,
        controller: u8,
        value: u8,
        sample_offset: i32,
    ) -> Result<()> {
        let event = unsafe {
            let mut event: Event = std::mem::zeroed();
            event.busIndex = 0;
            event.sampleOffset = sample_offset.max(0);
            event.ppqPosition = 0.0;
            event.flags = Event_::EventFlags_::kIsLive as u16;
            event.r#type = kLegacyMIDICCOutEvent as u16;

            event.__field0.midiCCOut.controlNumber = controller;
            event.__field0.midiCCOut.channel = channel.as_index() as std::os::raw::c_char;
            event.__field0.midiCCOut.value = value as std::os::raw::c_char;
            event.__field0.midiCCOut.value2 = 0;
            event
        };

        self.input_events.add_event(event);
        Ok(())
    }

    /// Activate all event buses for MIDI processing
    unsafe fn activate_event_buses(component: &ComPtr<IComponent>) -> Result<()> {
        // Activate event input buses
        let event_input_count = component.getBusCount(kEvent as i32, kInput as i32);
        for i in 0..event_input_count {
            let mut bus_info = std::mem::zeroed();
            let info_result = component.getBusInfo(kEvent as i32, kInput as i32, i, &mut bus_info);
            let name = if info_result == kResultOk {
                crate::internal::utils::vst_string_to_string(&bus_info.name)
            } else {
                format!("#{}", i)
            };

            let activate_result = component.activateBus(kEvent as i32, kInput as i32, i, 1);
            log::debug!(
                "Event Input Bus {} (index {}): activation result = {:#x}",
                name,
                i,
                activate_result
            );
        }

        // Activate event output buses
        let event_output_count = component.getBusCount(kEvent as i32, kOutput as i32);
        for i in 0..event_output_count {
            let mut bus_info = std::mem::zeroed();
            let info_result = component.getBusInfo(kEvent as i32, kOutput as i32, i, &mut bus_info);
            let name = if info_result == kResultOk {
                crate::internal::utils::vst_string_to_string(&bus_info.name)
            } else {
                format!("#{}", i)
            };

            let activate_result = component.activateBus(kEvent as i32, kOutput as i32, i, 1);
            log::debug!(
                "Event Output Bus {} (index {}): activation result = {:#x}",
                name,
                i,
                activate_result
            );
        }

        Ok(())
    }

    /// Get or create controller (handles both single-component and separate controller)
    unsafe fn get_or_create_controller(
        component: &ComPtr<IComponent>,
        factory: &ComPtr<IPluginFactory>,
        context: *mut FUnknown,
    ) -> Result<Option<ComPtr<IEditController>>> {
        // First, try to cast component to IEditController (single component)
        if let Some(controller) = component.cast::<IEditController>() {
            log::debug!("Component implements IEditController (single component)");
            return Ok(Some(controller));
        }

        // If not single component, try to get separate controller
        log::debug!("Component is separate from controller, getting controller class ID...");
        let mut controller_cid: [std::os::raw::c_char; 16] = [0; 16];
        let result = component.getControllerClassId(&mut controller_cid);

        if result != kResultOk {
            log::warn!("Failed to get controller class ID: {:#x}", result);
            return Ok(None);
        }

        log::debug!("Got controller class ID, creating controller...");
        let mut controller_ptr: *mut IEditController = ptr::null_mut();
        let create_result = factory.createInstance(
            controller_cid.as_ptr(),
            IEditController::IID.as_ptr() as *const std::os::raw::c_char,
            &mut controller_ptr as *mut _ as *mut _,
        );

        if create_result != kResultOk || controller_ptr.is_null() {
            log::warn!(
                "Failed to create controller: {:#x}, ptr is null: {}",
                create_result,
                controller_ptr.is_null()
            );
            return Ok(None);
        }

        let controller = ComPtr::<IEditController>::from_raw(controller_ptr)
            .ok_or_else(|| Error::InterfaceError("Failed to wrap controller".to_string()))?;

        // Initialize controller with the same host context as the component.
        log::debug!("Initializing controller...");
        let init_result = controller.initialize(context);
        if init_result != kResultOk {
            log::warn!("Failed to initialize controller: {:#x}", init_result);
            return Ok(None);
        }

        log::debug!("Controller created and initialized successfully");
        Ok(Some(controller))
    }

    /// Connect component and controller via IConnectionPoint
    unsafe fn connect_component_and_controller(
        component: &ComPtr<IComponent>,
        controller: &ComPtr<IEditController>,
    ) -> Result<()> {
        // Try to get connection points
        let comp_cp = component.cast::<IConnectionPoint>();
        let ctrl_cp = controller.cast::<IConnectionPoint>();

        if let (Some(comp_cp), Some(ctrl_cp)) = (comp_cp, ctrl_cp) {
            // Connect component to controller
            let result1 = comp_cp.connect(ctrl_cp.as_ptr());
            let result2 = ctrl_cp.connect(comp_cp.as_ptr());

            if result1 == kResultOk && result2 == kResultOk {
                log::debug!("Components connected successfully");
            } else {
                // Non-fatal: single-component plugins expose the same object as both
                // component and controller, so connecting it to itself fails — and the
                // connection is a best-effort messaging channel, not required for the
                // plugin to load and run. Log and continue rather than failing the load.
                log::warn!(
                    "Component connection not established (continuing): comp->ctrl={:#x}, ctrl->comp={:#x}",
                    result1,
                    result2
                );
            }
            Ok(())
        } else {
            log::debug!("Components do not support IConnectionPoint - might be single component");
            Ok(()) // Not an error - single components don't need connection
        }
    }

    /// Transfer component state to the controller. Unused: the controller is synced via
    /// `setComponentState` (in `load_state`) instead.
    #[allow(dead_code)]
    unsafe fn transfer_component_state(
        component: &ComPtr<IComponent>,
        controller: &ComPtr<IEditController>,
    ) -> Result<()> {
        // Get state from component
        let mut state_ptr: *mut vst3::Steinberg::IBStream = ptr::null_mut();

        // First check if component supports state saving
        let save_result = component.getState(&mut state_ptr as *mut _ as *mut _);

        if save_result != kResultOk || state_ptr.is_null() {
            log::debug!("Component does not provide state or state is empty");
            return Ok(()); // Not an error - some plugins don't have state
        }

        // Wrap the state stream
        let state_stream = ComPtr::<vst3::Steinberg::IBStream>::from_raw(state_ptr)
            .ok_or_else(|| Error::InterfaceError("Failed to wrap state stream".to_string()))?;

        // Set state on controller
        let set_result = controller.setComponentState(state_stream.as_ptr());

        if set_result == kResultOk {
            log::debug!("Component state transferred to controller successfully");
        } else {
            log::warn!(
                "Failed to set component state on controller: {:#x}",
                set_result
            );
        }

        Ok(())
    }
}

/// Map a sample offset within the caller's block onto one chunk of it.
///
/// Returns the offset rebased to the chunk (`0..frames`), or `None` when the offset belongs to
/// a different chunk. Anything scheduled past the end of the caller's block lands at the end of
/// the final chunk rather than being dropped — a note scheduled near `block_size` still sounds
/// when the device hands over a shorter block.
fn chunk_offset(
    sample_offset: i32,
    chunk_start: usize,
    frames: usize,
    is_last: bool,
) -> Option<i32> {
    let start = chunk_start as i64;
    let end = start + frames as i64;
    // A negative offset is undefined in VST3; treat it as the start of the block.
    let offset = i64::from(sample_offset.max(0));
    if offset < start {
        None
    } else if offset < end {
        Some((offset - start) as i32)
    } else if is_last {
        Some(frames.saturating_sub(1) as i32)
    } else {
        None
    }
}

/// Stage the events belonging to one chunk into the plugin-facing event list, rebasing each
/// offset to the chunk start (see [`chunk_offset`]). Replaces whatever the list held, so each
/// chunk of a split block sees exactly its own events, in order and with their spacing intact.
fn stage_chunk_events(
    queued: &[Event],
    list: &HostEventList,
    chunk_start: usize,
    frames: usize,
    is_last: bool,
) {
    list.reset_with(queued.iter().filter_map(|event| {
        chunk_offset(event.sampleOffset, chunk_start, frames, is_last).map(|offset| {
            let mut event = *event;
            event.sampleOffset = offset;
            event
        })
    }));
}

/// Format a VST3 class id (a raw 16-byte `TUID`) as the 32-hex-digit string this library uses
/// as a plugin's stable `uid`.
fn format_class_uid(cid: &[std::os::raw::c_char; 16]) -> String {
    cid.iter().map(|&b| format!("{:02X}", b as u8)).collect()
}

/// Ordered teardown for a component that has been initialized but whose `PluginImpl` doesn't
/// exist yet.
///
/// Between `IComponent::initialize` returning success and the finished `PluginImpl`, `load` can
/// still fail — and by then the plugin may have spawned threads and registered callbacks.
/// Releasing its interfaces and unloading the module without `terminate()` leaves that code
/// running inside memory the unload is about to unmap. This guard runs the same sequence
/// `Drop for PluginImpl` does, unless [`Self::disarm`] hands the job to the built plugin.
///
/// It holds its own references (COM refcounts), so `load` keeps using the originals as it
/// builds; the extra references are released when the guard drops either way.
struct InitializedComponent {
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    single_component: bool,
    armed: bool,
}

impl InitializedComponent {
    fn new(component: ComPtr<IComponent>) -> Self {
        Self {
            component,
            controller: None,
            single_component: false,
            armed: true,
        }
    }

    /// Record the controller so a later failure disconnects and terminates it too.
    fn attach_controller(
        &mut self,
        controller: Option<ComPtr<IEditController>>,
        single_component: bool,
    ) {
        self.controller = controller;
        self.single_component = single_component;
    }

    /// Cancel the teardown: the finished `PluginImpl` owns the lifecycle from here.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InitializedComponent {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        log::debug!("Plugin load failed after initialize; terminating the component");
        unsafe {
            terminate_component(
                &self.component,
                self.controller.as_ref(),
                self.single_component,
            );
        }
    }
}

/// Disconnect and terminate a live component/controller pair, in VST3 order.
///
/// The mirror of the load sequence: break the component↔controller connection, terminate the
/// controller, then terminate the component. Many plugins (dual-component ones especially) rely
/// on `terminate()` to break that link before release and crash without it. A single-component
/// plugin is one object exposed as both halves, so it has no connection pair and is terminated
/// once, as the component.
unsafe fn terminate_component(
    component: &ComPtr<IComponent>,
    controller: Option<&ComPtr<IEditController>>,
    single_component: bool,
) {
    if let Some(controller) = controller {
        if !single_component {
            if let (Some(comp_cp), Some(ctrl_cp)) = (
                component.cast::<IConnectionPoint>(),
                controller.cast::<IConnectionPoint>(),
            ) {
                comp_cp.disconnect(ctrl_cp.as_ptr());
                ctrl_cp.disconnect(comp_cp.as_ptr());
            }
            controller.terminate();
        }
    }
    component.terminate();
}

impl Drop for PluginImpl {
    fn drop(&mut self) {
        // VST3 teardown order: detach the editor, stop processing, deactivate the component,
        // disconnect the component/controller connection points, terminate the controller and the
        // component, then drop the COM references. Many plugins (dual-component ones especially)
        // rely on `terminate()` to break the controller↔component link before release and crash
        // without it.
        //
        // The editor goes first. A view that is still `attached()` when the controller is
        // terminated — and then merely Released as a field, without `removed()` — leaves the
        // plugin's platform frame and its idle timer alive, pointing at a host window the caller
        // is about to destroy. `open_editor` and `close_editor` pair attach/removed correctly, but
        // a host that drops the plugin with the editor open (or is unwound past its own close)
        // never reaches `close_editor`, so do it here too. It is a no-op when no view is attached.
        let _ = PluginInternal::close_editor(self);

        let _ = self.stop_processing();

        unsafe {
            if self.is_active {
                self.component.setActive(0);
                self.is_active = false;
            }

            terminate_component(
                &self.component,
                self.controller.as_ref(),
                self.single_component,
            );
        }
    }
}

/// The `ProcessContext.state` flags the host advertises each block: transport playing, with
/// a valid tempo, time signature, continuous sample time, and musical (quarter-note)
/// playhead. The last two are essential — without `kContTimeValid`/`kProjectTimeMusicValid`
/// a spec-conformant plugin treats the advancing `continousTimeSamples`/`projectTimeMusic`
/// (see [`advance_process_context`]) as invalid and ignores it. The `as u32` cast is needed
/// because the `StatesAndFlags_` constants are generated as `i32` on some targets (Windows)
/// and `u32` on others (macOS).
#[allow(clippy::unnecessary_cast)] // the `as u32` is needed where the constants are i32 (Windows)
const PROCESS_CONTEXT_STATE: u32 = (ProcessContext_::StatesAndFlags_::kPlaying
    | ProcessContext_::StatesAndFlags_::kTempoValid
    | ProcessContext_::StatesAndFlags_::kTimeSigValid
    | ProcessContext_::StatesAndFlags_::kContTimeValid
    | ProcessContext_::StatesAndFlags_::kProjectTimeMusicValid)
    as u32;

/// The `kPlaying` bit on its own, factored out of [`PROCESS_CONTEXT_STATE`] so the playing
/// state can be toggled at runtime without disturbing the validity flags.
#[allow(clippy::unnecessary_cast)] // the `as u32` is needed where the constant is i32 (Windows)
const PROCESS_CONTEXT_PLAYING: u32 = ProcessContext_::StatesAndFlags_::kPlaying as u32;

/// Compute the `ProcessContext.state` flags for a given playing state: the validity flags are
/// always set; `kPlaying` is included only when the transport is playing.
fn process_context_state(playing: bool) -> u32 {
    if playing {
        PROCESS_CONTEXT_STATE | PROCESS_CONTEXT_PLAYING
    } else {
        PROCESS_CONTEXT_STATE & !PROCESS_CONTEXT_PLAYING
    }
}

/// Advance the transport in a `ProcessContext` by `frames` samples after a processed block.
/// Keeps `continousTimeSamples`/`projectTimeSamples` (and the musical playhead derived from
/// the current tempo) moving so tempo-synced plugins don't see a frozen time-0.
fn advance_process_context(ctx: &mut ProcessContext, frames: i64) {
    ctx.continousTimeSamples = ctx.continousTimeSamples.wrapping_add(frames);
    ctx.projectTimeSamples = ctx.projectTimeSamples.wrapping_add(frames);
    if ctx.sampleRate > 0.0 {
        // Quarter notes elapsed = seconds * (BPM / 60).
        let secs = ctx.projectTimeSamples as f64 / ctx.sampleRate;
        ctx.projectTimeMusic = secs * (ctx.tempo / 60.0);
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    #[allow(clippy::unnecessary_cast)] // `as u32` needed where the constants are i32 (Windows)
    fn process_context_state_advertises_playhead_validity() {
        // The advancing continous/musical playhead is only honored by conformant plugins if
        // its validity flags are set.
        use ProcessContext_::StatesAndFlags_ as F;
        assert_ne!(PROCESS_CONTEXT_STATE & F::kPlaying as u32, 0);
        assert_ne!(PROCESS_CONTEXT_STATE & F::kTempoValid as u32, 0);
        assert_ne!(PROCESS_CONTEXT_STATE & F::kTimeSigValid as u32, 0);
        assert_ne!(PROCESS_CONTEXT_STATE & F::kContTimeValid as u32, 0);
        assert_ne!(PROCESS_CONTEXT_STATE & F::kProjectTimeMusicValid as u32, 0);
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // `as u32` needed where the constants are i32 (Windows)
    fn process_context_state_toggles_playing_without_disturbing_validity() {
        use ProcessContext_::StatesAndFlags_ as F;
        let playing = process_context_state(true);
        let stopped = process_context_state(false);
        // kPlaying tracks the playing flag.
        assert_ne!(playing & F::kPlaying as u32, 0);
        assert_eq!(stopped & F::kPlaying as u32, 0);
        // The validity flags survive in both states.
        for flag in [
            F::kTempoValid as u32,
            F::kTimeSigValid as u32,
            F::kContTimeValid as u32,
            F::kProjectTimeMusicValid as u32,
        ] {
            assert_ne!(playing & flag, 0);
            assert_ne!(stopped & flag, 0);
        }
    }

    #[test]
    fn advance_moves_playhead_and_musical_time() {
        let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
        ctx.sampleRate = 48_000.0;
        ctx.tempo = 120.0;
        // One second of audio at 48 kHz in 512-sample blocks.
        let blocks = 48_000 / 512;
        for _ in 0..blocks {
            advance_process_context(&mut ctx, 512);
        }
        let advanced = (blocks * 512) as i64;
        assert_eq!(ctx.projectTimeSamples, advanced);
        assert_eq!(ctx.continousTimeSamples, advanced);
        // ~0.992 s elapsed at 120 BPM → ~1.98 quarter notes; just assert it moved forward.
        assert!(ctx.projectTimeMusic > 1.9 && ctx.projectTimeMusic < 2.1);
    }
}

#[cfg(test)]
mod chunked_block_tests {
    use super::*;
    use crate::internal::com_implementations::create_event_list;

    fn note_on_at(offset: i32, pitch: i16) -> Event {
        let mut e: Event = unsafe { std::mem::zeroed() };
        e.r#type = kNoteOnEvent as u16;
        e.sampleOffset = offset;
        e.__field0.noteOn.pitch = pitch;
        e
    }

    /// Read back what the plugin would see: `(sampleOffset, pitch)` per staged event.
    fn staged(list: &HostEventList) -> Vec<(i32, i16)> {
        let mut out = Vec::new();
        list.for_each_then_clear(|e| {
            out.push((e.sampleOffset, unsafe { e.__field0.noteOn.pitch }))
        });
        out
    }

    /// A device block larger than the configured block size is split into chunks, and each
    /// queued event has to land in the chunk that actually contains its offset — rebased to
    /// that chunk. Routing everything to chunk 0 (clamped to its end) fires late events ~20 ms
    /// early and collapses the spacing between them.
    #[test]
    fn events_are_routed_to_the_chunk_that_contains_them() {
        // A 2048-frame device block processed in 512-frame chunks.
        let queued = [
            note_on_at(0, 60),    // chunk 0, offset 0
            note_on_at(100, 61),  // chunk 0, offset 100
            note_on_at(512, 62),  // chunk 1, offset 0 (first sample of the chunk)
            note_on_at(1500, 63), // chunk 2, offset 476
        ];
        let list = create_event_list();

        stage_chunk_events(&queued, &list, 0, 512, false);
        assert_eq!(staged(&list), vec![(0, 60), (100, 61)]);

        stage_chunk_events(&queued, &list, 512, 512, false);
        assert_eq!(staged(&list), vec![(0, 62)]);

        stage_chunk_events(&queued, &list, 1024, 512, false);
        assert_eq!(staged(&list), vec![(476, 63)]);

        // The final chunk holds nothing: every event was delivered exactly once, earlier.
        stage_chunk_events(&queued, &list, 1536, 512, true);
        assert_eq!(staged(&list), Vec::new());
    }

    /// Offsets past the end of the caller's block still have to sound; they collapse into the
    /// last chunk rather than being dropped (the pre-split behaviour, preserved).
    #[test]
    fn offsets_past_the_block_land_in_the_final_chunk() {
        let queued = [note_on_at(9_000, 64), note_on_at(-5, 65)];
        let list = create_event_list();

        // A non-final chunk ignores the overshoot; the negative offset is treated as 0.
        stage_chunk_events(&queued, &list, 0, 512, false);
        assert_eq!(staged(&list), vec![(0, 65)]);

        // The final chunk absorbs it at its last sample.
        stage_chunk_events(&queued, &list, 512, 512, true);
        assert_eq!(staged(&list), vec![(511, 64)]);
    }

    /// A block that fits in one chunk keeps the old semantics: exact offsets inside the block,
    /// anything past the end clamped to its last sample.
    #[test]
    fn single_chunk_block_clamps_to_its_own_length() {
        let queued = [note_on_at(0, 60), note_on_at(200, 61), note_on_at(999, 62)];
        let list = create_event_list();

        stage_chunk_events(&queued, &list, 0, 256, true);
        assert_eq!(staged(&list), vec![(0, 60), (200, 61), (255, 62)]);
    }

    #[test]
    fn chunk_offset_maps_each_offset_once() {
        // Every offset in a 3-chunk block belongs to exactly one chunk.
        let chunks = [
            (0usize, 512usize, false),
            (512, 512, false),
            (1024, 512, true),
        ];
        for offset in [0, 1, 511, 512, 1023, 1024, 1535] {
            let hits: Vec<_> = chunks
                .iter()
                .filter_map(|&(start, frames, last)| {
                    chunk_offset(offset, start, frames, last).map(|o| (start, o))
                })
                .collect();
            assert_eq!(hits.len(), 1, "offset {offset} routed to {hits:?}");
            let (start, rebased) = hits[0];
            assert_eq!(start as i32 + rebased, offset);
        }
    }
}

#[cfg(test)]
mod output_midi_tests {
    use super::*;

    fn blank_event() -> Event {
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn converts_note_on() {
        let mut e = blank_event();
        e.r#type = kNoteOnEvent as u16;
        e.__field0.noteOn.channel = 0;
        e.__field0.noteOn.pitch = 60;
        e.__field0.noteOn.velocity = 1.0;
        assert_eq!(
            event_to_midi(&e),
            Some(MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 60,
                velocity: 127
            })
        );
    }

    #[test]
    fn converts_note_off() {
        let mut e = blank_event();
        e.r#type = kNoteOffEvent as u16;
        e.__field0.noteOff.channel = 1;
        e.__field0.noteOff.pitch = 64;
        e.__field0.noteOff.velocity = 0.0;
        assert_eq!(
            event_to_midi(&e),
            Some(MidiEvent::NoteOff {
                channel: MidiChannel::Ch2,
                note: 64,
                velocity: 0
            })
        );
    }

    #[test]
    fn converts_legacy_cc_and_pitchbend() {
        // A plain CC.
        let mut cc = blank_event();
        cc.r#type = kLegacyMIDICCOutEvent as u16;
        cc.__field0.midiCCOut.controlNumber = 1; // mod wheel
        cc.__field0.midiCCOut.channel = 0;
        cc.__field0.midiCCOut.value = 64;
        assert_eq!(
            event_to_midi(&cc),
            Some(MidiEvent::ControlChange {
                channel: MidiChannel::Ch1,
                controller: 1,
                value: 64
            })
        );

        // Pitch bend round-trips the 14-bit value (LSB in value, MSB in value2).
        let mut pb = blank_event();
        pb.r#type = kLegacyMIDICCOutEvent as u16;
        pb.__field0.midiCCOut.controlNumber = ControllerNumbers_::kPitchBend as u8;
        pb.__field0.midiCCOut.channel = 0;
        pb.__field0.midiCCOut.value = (10000 & 0x7F) as std::os::raw::c_char;
        pb.__field0.midiCCOut.value2 = ((10000 >> 7) & 0x7F) as std::os::raw::c_char;
        assert_eq!(
            event_to_midi(&pb),
            Some(MidiEvent::PitchBend {
                channel: MidiChannel::Ch1,
                value: 10000
            })
        );
    }

    #[test]
    fn ignores_unknown_event_types() {
        let mut e = blank_event();
        e.r#type = 9999;
        assert_eq!(event_to_midi(&e), None);
    }
}
