// Copyright 2018 Google LLC, licensed under http://www.apache.org/licenses/LICENSE-2.0

use abstutil;
use dimensioned::si;
use geom::{Line, PolyLine, Pt2D};
use std::collections::BTreeMap;
use std::fmt;
use LaneID;

// TODO reconsider pub usize. maybe outside world shouldnt know.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BuildingID(pub usize);

impl fmt::Display for BuildingID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BuildingID({0})", self.0)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FrontPath {
    pub bldg: BuildingID,
    pub sidewalk: LaneID,
    // Goes from the building to the sidewalk
    pub line: Line,
    pub dist_along_sidewalk: si::Meter<f64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Building {
    pub id: BuildingID,
    pub points: Vec<Pt2D>,
    pub osm_tags: BTreeMap<String, String>,
    pub osm_way_id: i64,

    pub front_path: FrontPath,
}

impl PartialEq for Building {
    fn eq(&self, other: &Building) -> bool {
        self.id == other.id
    }
}

impl Building {
    pub fn dump_debug(&self) {
        println!("{}", abstutil::to_json(self));
        println!("{}", PolyLine::new(self.points.clone()));
    }
}
