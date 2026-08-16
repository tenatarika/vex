use super::Capabilities;

pub fn current() -> Capabilities {
    Capabilities {
        signals: true,
        empty_reason: false,
        // Phase 13.2 — `vex bundle` modes. Order MUST mirror
        // `crate::cli::cmd_bundle::BundleModeFlag::ALL` (the unit test
        // `bundle_mode_flag_all_matches_capabilities` pins this).
        bundle_modes: crate::cli::cmd_bundle::BundleModeFlag::ALL.to_vec(),
        why: true,
        scope_filters: true,
        metadata_filters: true,
        auto_update: true,
        async_update: true,
        history_diff: true,
        structured_result_kind: true,
        result_completeness: true,
    }
}
