use super::Capabilities;

pub fn current() -> Capabilities {
    Capabilities {
        signals: true,
        empty_reason: false,
        bundle_modes: vec![],
        why: true,
        scope_filters: true,
        metadata_filters: true,
        auto_update: true,
    }
}
