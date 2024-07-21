use utils::id::TimelineId;

#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AncestorDetached {
    pub reparented_timelines: Vec<TimelineId>,
}
