//! go-file: pkg/meta/model/resource_group.go

use std::fmt::Write;

use tidb_base::gostd::time::{MILLISECOND, duration_string};
use tidb_parser::model::{
    MediumPriorityValue, RunawayActionSwitchGroup, WatchNone, priority_value_to_name,
};

use crate::placement::{
    write_setting_duration_to_builder, write_setting_integer_to_builder,
    write_setting_item_to_builder, write_setting_string_to_builder,
};
use crate::{ResourceGroupInfo, ResourceGroupSettings};

/// NewResourceGroupSettings creates a new ResourceGroupSettings.
// go: NewResourceGroupSettings L1
pub fn new_resource_group_settings() -> ResourceGroupSettings {
    ResourceGroupSettings {
        ru_rate: 0,
        priority: MediumPriorityValue,
        cpu_limiter: String::new(),
        io_read_bandwidth: String::new(),
        io_write_bandwidth: String::new(),
        burst_limit: 0,
        ..ResourceGroupSettings::default()
    }
}

impl ResourceGroupSettings {
    /// String implements fmt.Stringer.
    // go: ResourceGroupSettings.String L1 [M-NAME, M-INT]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut sb = String::new();
        let sep = Some(", ");
        if self.ru_rate != 0 {
            write_setting_integer_to_builder(&mut sb, "RU_PER_SEC", self.ru_rate, sep);
        }
        write_setting_item_to_builder(
            &mut sb,
            &format!("PRIORITY={}", priority_value_to_name(self.priority)),
            sep,
        );
        if !self.cpu_limiter.is_empty() {
            write_setting_string_to_builder(&mut sb, "CPU", &self.cpu_limiter, sep);
        }
        if !self.io_read_bandwidth.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "IO_READ_BANDWIDTH",
                &self.io_read_bandwidth,
                sep,
            );
        }
        if !self.io_write_bandwidth.is_empty() {
            write_setting_string_to_builder(
                &mut sb,
                "IO_WRITE_BANDWIDTH",
                &self.io_write_bandwidth,
                sep,
            );
        }
        // Once burst limit is negative, meaning allow burst with unlimit.
        if self.burst_limit < 0 {
            write_setting_item_to_builder(&mut sb, "BURSTABLE", sep);
        }
        if let Some(r) = &self.runaway {
            sb.push_str(", QUERY_LIMIT=(");
            // rule settings
            let mut first_param = true;
            if r.exec_elapsed_time_ms > 0 {
                let d = (r.exec_elapsed_time_ms as i64).wrapping_mul(MILLISECOND);
                let _ = write!(sb, "EXEC_ELAPSED=\"{}\"", duration_string(d));
                first_param = false;
            }
            if r.processed_keys > 0 {
                if !first_param {
                    sb.push(' ');
                }
                let _ = write!(sb, "PROCESSED_KEYS={}", r.processed_keys);
                first_param = false;
            }
            if r.request_unit > 0 {
                if !first_param {
                    sb.push(' ');
                }
                let _ = write!(sb, "RU={}", r.request_unit);
            }
            // action settings
            if r.action == RunawayActionSwitchGroup {
                write_setting_item_to_builder(
                    &mut sb,
                    &format!("ACTION={}({})", r.action.to_string(), r.switch_group_name),
                    None,
                );
            } else {
                write_setting_item_to_builder(
                    &mut sb,
                    &format!("ACTION={}", r.action.to_string()),
                    None,
                );
            }
            if r.watch_type != WatchNone {
                write_setting_item_to_builder(
                    &mut sb,
                    &format!("WATCH={}", r.watch_type.to_string()),
                    None,
                );
                if r.watch_duration_ms > 0 {
                    write_setting_duration_to_builder(
                        &mut sb,
                        "DURATION",
                        r.watch_duration_ms.wrapping_mul(MILLISECOND),
                        None,
                    );
                } else {
                    write_setting_item_to_builder(&mut sb, "DURATION=UNLIMITED", None);
                }
            }
            sb.push(')');
        }
        if let Some(b) = &self.background {
            sb.push_str(", BACKGROUND=(");
            let mut first = true;
            if b.job_types.as_ref().is_some_and(|j| !j.is_empty()) {
                let _ = write!(
                    sb,
                    "TASK_TYPES='{}'",
                    b.job_types.as_deref().unwrap_or(&[]).join(",")
                );
                first = false;
            }
            if b.resource_util_limit > 0 {
                if !first {
                    sb.push_str(", ");
                }
                let _ = write!(sb, "UTILIZATION_LIMIT={}", b.resource_util_limit);
            }
            sb.push(')');
        }
        sb
    }

    /// Adjust adjusts the resource group settings: the burst limit follows `RU_PER_SEC`
    /// unless burstable.
    // go: ResourceGroupSettings.Adjust L1
    pub fn adjust(&mut self) {
        // Curretly we only support ru_per_sec sytanx, so BurstLimit(capicity) is always same
        // as ru_per_sec except burstable.
        if self.burst_limit >= 0 {
            self.burst_limit = self.ru_rate as i64;
        }
    }
}

impl Clone for ResourceGroupSettings {
    /// Clone clones the settings (a struct copy).
    // go: ResourceGroupSettings.Clone L1
    fn clone(&self) -> ResourceGroupSettings {
        self.struct_copy()
    }
}

impl Clone for ResourceGroupInfo {
    /// Clone clones the ResourceGroupInfo. Panics if the settings are nil, as Go does.
    // go: ResourceGroupInfo.Clone L1 [M-PANIC]
    fn clone(&self) -> ResourceGroupInfo {
        let mut cloned = self.struct_copy();
        cloned.resource_group_settings = Some(Box::new(
            self.resource_group_settings
                .as_ref()
                .expect("invalid memory address or nil pointer dereference")
                .as_ref()
                .clone(),
        ));
        cloned
    }
}
