use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{LogicalRect, OutputId, WindowId};

/// Selects the foreground desktop source to observe or capture.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSelector {
    Output {
        output_id: OutputId,
    },
    Window {
        window_id: WindowId,
    },
    Region {
        output_id: OutputId,
        region: LogicalRect,
    },
}
