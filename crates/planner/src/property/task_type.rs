//! go-file: pkg/planner/property/task_type.go

#![allow(non_upper_case_globals)]

/// TaskType is the type of execution task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskType(pub isize);

/// RootTaskType stands for the tasks that executed in the TiDB layer.
pub const RootTaskType: TaskType = TaskType(0);

/// CopSingleReadTaskType stands for the a TableScan or IndexScan tasks
/// executed in the coprocessor layer.
pub const CopSingleReadTaskType: TaskType = TaskType(1);

/// CopMultiReadTaskType stands for the a IndexLookup tasks executed in the
/// coprocessor layer.
pub const CopMultiReadTaskType: TaskType = TaskType(2);

/// MppTaskType stands for task that would run on Mpp nodes, currently meaning the tiflash node.
pub const MppTaskType: TaskType = TaskType(3);

impl TaskType {
    /// String implements fmt.Stringer interface.
    // go: TaskType.String L1 [M-NAME]
    pub fn to_string(&self) -> &'static str {
        match *self {
            RootTaskType => "rootTask",
            CopSingleReadTaskType => "copSingleReadTask",
            CopMultiReadTaskType => "copMultiReadTask",
            MppTaskType => "mppTask",
            _ => "UnknownTaskType",
        }
    }
}
