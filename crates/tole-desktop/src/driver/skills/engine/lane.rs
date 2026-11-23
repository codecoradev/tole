//! The schedule of one route of the skills driver: the shared lane, keyed by
//! the skills driver's own load ids.

use crate::adapter::skills::LoadId;

pub(super) type Lane = crate::driver::shared::lane::Lane<LoadId>;
