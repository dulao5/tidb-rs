//! go-file: pkg/meta/model/placement.go

use tidb_base::gostd::time::{Duration, duration_string};

use crate::{PlacementSettings, PolicyInfo};

impl PlacementSettings {
    /// String implements fmt.Stringer: the settings as `PRIMARY_REGION="..." ...`.
    // go: PlacementSettings.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut sb = String::new();
        if !self.primary_region.is_empty() {
            write_setting_string_to_builder(&mut sb, "PRIMARY_REGION", &self.primary_region, None);
        }
        if !self.regions.is_empty() {
            write_setting_string_to_builder(&mut sb, "REGIONS", &self.regions, None);
        }
        if !self.schedule.is_empty() {
            write_setting_string_to_builder(&mut sb, "SCHEDULE", &self.schedule, None);
        }
        if !self.constraints.is_empty() {
            write_setting_string_to_builder(&mut sb, "CONSTRAINTS", &self.constraints, None);
        }
        if !self.leader_constraints.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "LEADER_CONSTRAINTS",
                &self.leader_constraints,
                None,
            );
        }
        if self.voters > 0 {
            write_setting_integer_to_builder(&mut sb, "VOTERS", self.voters, None);
        }
        if !self.voter_constraints.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "VOTER_CONSTRAINTS",
                &self.voter_constraints,
                None,
            );
        }
        if self.followers > 0 {
            write_setting_integer_to_builder(&mut sb, "FOLLOWERS", self.followers, None);
        }
        if !self.follower_constraints.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "FOLLOWER_CONSTRAINTS",
                &self.follower_constraints,
                None,
            );
        }
        if self.learners > 0 {
            write_setting_integer_to_builder(&mut sb, "LEARNERS", self.learners, None);
        }
        if !self.learner_constraints.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "LEARNER_CONSTRAINTS",
                &self.learner_constraints,
                None,
            );
        }
        if !self.survival_preferences.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "SURVIVAL_PREFERENCES",
                &self.survival_preferences,
                None,
            );
        }
        sb
    }
}

impl Clone for PlacementSettings {
    /// Clone clones the placement settings (a struct copy).
    // go: PlacementSettings.Clone L1
    fn clone(&self) -> PlacementSettings {
        self.struct_copy()
    }
}

/// The separator of the `writeSetting*` helpers: Go's variadic `separatorFns ...func()`
/// (the callers pass zero or one). `None` writes a single space.
pub(crate) type Separator<'a> = Option<&'a str>;

/// writeSettingStringToBuilder writes `item="value"` with `"` escaped.
// go: writeSettingStringToBuilder L2 (separatorFns as an optional separator string)
pub(crate) fn write_setting_string_to_builder(
    sb: &mut String,
    item: &str,
    value: &str,
    sep: Separator<'_>,
) {
    write_setting_item_to_builder(
        sb,
        &format!("{item}=\"{}\"", value.replace('"', "\\\"")),
        sep,
    );
}

/// writeSettingIntegerToBuilder writes `item=value`.
// go: writeSettingIntegerToBuilder L2 (separatorFns as an optional separator string)
pub(crate) fn write_setting_integer_to_builder(
    sb: &mut String,
    item: &str,
    value: u64,
    sep: Separator<'_>,
) {
    write_setting_item_to_builder(sb, &format!("{item}={value}"), sep);
}

/// writeSettingDurationToBuilder writes `item="<duration>"`.
// go: writeSettingDurationToBuilder L2 (separatorFns as an optional separator string)
pub(crate) fn write_setting_duration_to_builder(
    sb: &mut String,
    item: &str,
    dur: Duration,
    sep: Separator<'_>,
) {
    write_setting_string_to_builder(sb, item, &duration_string(dur), sep);
}

/// writeSettingItemToBuilder writes the separator (unless the builder is empty), then the
/// item.
// go: writeSettingItemToBuilder L2 (separatorFns as an optional separator string)
pub(crate) fn write_setting_item_to_builder(sb: &mut String, item: &str, sep: Separator<'_>) {
    if !sb.is_empty() {
        sb.push_str(sep.unwrap_or(" "));
    }
    sb.push_str(item);
}

impl Clone for PolicyInfo {
    /// Clone clones PolicyInfo. Panics if `PlacementSettings` is nil, as Go does (it
    /// dereferences the nil pointer).
    // go: PolicyInfo.Clone L1 [M-PANIC]
    fn clone(&self) -> PolicyInfo {
        let mut cloned = self.struct_copy();
        cloned.placement_settings = Some(Box::new(
            self.placement_settings
                .as_ref()
                .expect("invalid memory address or nil pointer dereference")
                .as_ref()
                .clone(),
        ));
        cloned
    }
}
