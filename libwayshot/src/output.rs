use crate::region::{LogicalRegion, Position, Size};
use std::fmt::Display;
use std::sync::OnceLock;
use wayland_client::protocol::{wl_output, wl_output::WlOutput};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1;

/// Represents an accessible wayland output.
///
/// Do not instantiate, instead use [`crate::WayshotConnection::get_all_outputs`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputInfo {
    pub output: WlOutput,
    pub name: String,
    pub description: String,
	pub transform: wl_output::Transform,
	pub physical_size: Size,
    pub logical_region: LogicalRegion,
	pub xdg_output: Option<ZxdgOutputV1>,
    pub scale: i32,
}

impl Display for OutputInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{name} ({description})",
            name = self.name,
            description = self.description
        )
    }
}

impl OutputInfo {
	/// The name of the output or maybe the screen?
	pub fn name(&self) -> &str {
		&self.name
	}

	pub(crate) fn new(output: WlOutput) -> Self {
		Self {
			output,
			logical_region: LogicalRegion::default(),
			physical_size: Size::default(),
			name: "".to_owned(),
			description: "".to_owned(),
			xdg_output: None,
			transform: wl_output::Transform::Normal,
			scale: 1,
		}
	}
}
