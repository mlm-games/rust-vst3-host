//! What a plugin is told about the state it is being handed.
//!
//! VST3 lets a plugin ask where a `setState` blob came from: the host attaches an
//! `IStreamAttributes` list to the stream, and the plugin reads `PresetAttributes::kStateType`
//! (and, for a preset, `PresetAttributes::kFilePathStringType`) off it. The SDK's
//! `Vst::Helpers::isProjectState()` reads exactly those keys.
//!
//! These tests cover the two halves of that contract reachable from outside the crate: the
//! public [`StateContext`] surface hosts build, and the isolation wire that has to carry it
//! unchanged — including what happens when one side of the boundary predates the field. The
//! stream attributes themselves are asserted where the streams live, in
//! `internal::com_implementations`.

use std::path::Path;
use vst3_host::plugin::StateContext;

// --- the public contract -------------------------------------------------------------

/// A restore with no stated context is a session restore: that is what every host that never
/// heard of preset context has been doing, and what `Plugin::load_state` keeps meaning.
#[test]
fn the_default_context_is_a_project_restore() {
    assert_eq!(StateContext::default(), StateContext::Project);
    assert_eq!(StateContext::Project.file_path(), None);
}

#[test]
fn a_preset_context_remembers_the_file_it_came_from() {
    let context = StateContext::preset_from_path("/Users/me/Presets/Big Lead.vstpreset");
    assert_eq!(
        context.file_path(),
        Some(Path::new("/Users/me/Presets/Big Lead.vstpreset"))
    );
    assert_eq!(
        context,
        StateContext::Preset {
            path: Some("/Users/me/Presets/Big Lead.vstpreset".to_string())
        }
    );
}

/// Preset bytes a host holds in memory are still preset bytes. The context says so without
/// inventing a path the plugin would then publish as real.
#[test]
fn a_preset_context_without_a_file_reports_no_path() {
    let context = StateContext::preset();
    assert_eq!(context, StateContext::Preset { path: None });
    assert_eq!(context.file_path(), None);
}

/// `Path` is not `Display`-able losslessly and cannot be JSON on every platform, so the
/// conversion happens once, at construction, and round-trips for ordinary paths.
#[test]
fn a_relative_path_survives_the_round_trip_through_the_context() {
    let context = StateContext::preset_from_path(Path::new("presets/init.vstpreset"));
    assert_eq!(
        context.file_path(),
        Some(Path::new("presets/init.vstpreset"))
    );
}

// --- the isolation wire --------------------------------------------------------------

#[cfg(feature = "process-isolation")]
mod wire {
    use super::*;
    use vst3_host::process_isolation::HostCommand;

    fn round_trip(command: &HostCommand) -> HostCommand {
        let json = serde_json::to_string(command).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn load_state(context: StateContext) -> HostCommand {
        HostCommand::LoadState {
            data: vec![0, 1, 2, 250, 255, 42],
            context,
        }
    }

    #[test]
    fn every_restore_context_survives_the_wire_unchanged() {
        for context in [
            StateContext::Project,
            StateContext::preset(),
            StateContext::preset_from_path("/Users/me/Presets/Big Lead.vstpreset"),
        ] {
            let sent = load_state(context.clone());
            match round_trip(&sent) {
                HostCommand::LoadState {
                    data,
                    context: received,
                } => {
                    assert_eq!(data, vec![0, 1, 2, 250, 255, 42]);
                    assert_eq!(received, context, "context changed across the wire");
                }
                other => panic!("LoadState round-trip changed the variant: {other:?}"),
            }
        }
    }

    /// Version skew, new helper reading an old host: a `LoadState` written before the context
    /// existed must still load, as the project restore it has always been.
    #[test]
    fn a_load_state_without_a_context_defaults_to_a_project_restore() {
        let legacy = r#"{"LoadState":{"data":[1,2,3]}}"#;
        match serde_json::from_str::<HostCommand>(legacy).expect("deserialize legacy LoadState") {
            HostCommand::LoadState { data, context } => {
                assert_eq!(data, vec![1, 2, 3]);
                assert_eq!(context, StateContext::Project);
            }
            other => panic!("legacy LoadState changed the variant: {other:?}"),
        }
    }

    /// Version skew, old helper reading a new host: the command carries a field the old helper
    /// has never seen. Unknown fields must be ignored rather than failing the whole command —
    /// the old helper then does the project restore it would have done anyway.
    #[test]
    fn an_unknown_field_on_a_load_state_is_ignored_rather_than_fatal() {
        let from_the_future =
            r#"{"LoadState":{"data":[1,2,3],"context":"Project","provenance":"a-future-field"}}"#;
        match serde_json::from_str::<HostCommand>(from_the_future)
            .expect("an unknown field must not break the command")
        {
            HostCommand::LoadState { data, context } => {
                assert_eq!(data, vec![1, 2, 3]);
                assert_eq!(context, StateContext::Project);
            }
            other => panic!("LoadState changed the variant: {other:?}"),
        }
    }

    /// The preset path is the one part of the context that carries caller data, so pin its
    /// encoding: a plain string under `path`, not an escaped blob or a dropped field.
    #[test]
    fn a_preset_path_crosses_as_readable_text() {
        let json = serde_json::to_string(&load_state(StateContext::preset_from_path(
            "/Users/me/Presets/Big Lead.vstpreset",
        )))
        .expect("serialize");
        assert!(
            json.contains(r#""path":"/Users/me/Presets/Big Lead.vstpreset""#),
            "the preset path must cross as text, got {json}"
        );

        let pathless =
            serde_json::to_string(&load_state(StateContext::preset())).expect("serialize");
        assert!(
            pathless.contains(r#""path":null"#),
            "a pathless preset must still be marked a preset, got {pathless}"
        );
    }
}
