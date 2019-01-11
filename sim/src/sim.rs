use crate::driving::DrivingSimState;
use crate::instrument::capture_backtrace;
use crate::intersections::IntersectionSimState;
use crate::parking::ParkingSimState;
use crate::scheduler::Scheduler;
use crate::spawn::Spawner;
use crate::transit::TransitSimState;
use crate::trips::TripManager;
use crate::view::WorldView;
use crate::walking::WalkingSimState;
use crate::{
    AgentID, CarID, Distance, Event, ParkedCar, PedestrianID, SimStats, Tick, TripID, TIMESTEP,
};
use abstutil;
use abstutil::Error;
use derivative::Derivative;
use map_model::{BuildingID, IntersectionID, LaneID, LaneType, Map, Path, Trace, Turn};
use rand::{FromEntropy, SeedableRng};
use rand_xorshift::XorShiftRng;
use serde_derive::{Deserialize, Serialize};
use std;
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Derivative)]
#[derivative(PartialEq)]
pub struct Sim {
    // TODO all the pub(crate) stuff is for helpers. Find a better solution.

    // This is slightly dangerous, but since we'll be using comparisons based on savestating (which
    // captures the RNG), this should be OK for now.
    #[derivative(PartialEq = "ignore")]
    pub(crate) rng: XorShiftRng,
    pub time: Tick,
    pub(crate) map_name: String,
    pub(crate) edits_name: String,
    // Some tests deliberately set different scenario names for comparisons.
    #[derivative(PartialEq = "ignore")]
    run_name: String,
    // TODO not quite the right type to represent durations
    savestate_every: Option<Tick>,

    stats: SimStats,

    pub(crate) spawner: Spawner,
    scheduler: Scheduler,
    intersection_state: IntersectionSimState,
    pub(crate) driving_state: DrivingSimState,
    pub(crate) parking_state: ParkingSimState,
    pub(crate) walking_state: WalkingSimState,
    pub(crate) transit_state: TransitSimState,
    pub(crate) trips_state: TripManager,

    // This should only be Some in the middle of step(). The caller of step() can grab this if
    // step() panics.
    current_agent_for_debugging: Option<AgentID>,
}

impl Sim {
    // TODO Options struct might be nicer, especially since we could glue it to structopt?
    pub fn new(
        map: &Map,
        run_name: String,
        rng_seed: Option<u8>,
        savestate_every: Option<Tick>,
    ) -> Sim {
        let mut rng = XorShiftRng::from_entropy();
        if let Some(seed) = rng_seed {
            rng = XorShiftRng::from_seed([seed; 16]);
        }

        Sim {
            rng,
            driving_state: DrivingSimState::new(),
            spawner: Spawner::empty(),
            scheduler: Scheduler::new(),
            trips_state: TripManager::new(),
            intersection_state: IntersectionSimState::new(map),
            parking_state: ParkingSimState::new(map),
            walking_state: WalkingSimState::new(),
            transit_state: TransitSimState::new(),
            time: Tick::zero(),
            map_name: map.get_name().to_string(),
            edits_name: map.get_edits().edits_name.to_string(),
            run_name,
            savestate_every,
            current_agent_for_debugging: None,
            stats: SimStats::new(Tick::zero()),
        }
    }

    pub fn load_savestate(
        path: String,
        new_run_name: Option<String>,
    ) -> Result<Sim, std::io::Error> {
        info!("Loading {}", path);
        abstutil::read_json(&path).map(|mut s: Sim| {
            if let Some(name) = new_run_name {
                s.run_name = name;
            }
            s
        })
    }

    pub fn edit_lane_type(&mut self, id: LaneID, old_type: LaneType, map: &Map) {
        match old_type {
            LaneType::Driving | LaneType::Bus | LaneType::Biking => {
                self.driving_state.edit_remove_lane(id)
            }
            LaneType::Parking => self.parking_state.edit_remove_lane(id),
            LaneType::Sidewalk => self.walking_state.edit_remove_lane(id),
        };
        let l = map.get_l(id);
        match l.lane_type {
            LaneType::Driving | LaneType::Bus | LaneType::Biking => {
                self.driving_state.edit_add_lane(id)
            }
            LaneType::Parking => self.parking_state.edit_add_lane(l),
            LaneType::Sidewalk => self.walking_state.edit_add_lane(id),
        };
    }

    pub fn edit_remove_turn(&mut self, t: &Turn) {
        if t.between_sidewalks() {
            self.walking_state.edit_remove_turn(t.id);
        } else {
            self.driving_state.edit_remove_turn(t.id);
        }
    }

    pub fn edit_add_turn(&mut self, t: &Turn) {
        if t.between_sidewalks() {
            self.walking_state.edit_add_turn(t.id);
        } else {
            self.driving_state.edit_add_turn(t.id);
        }
    }

    pub fn dump_before_abort(&self) {
        error!("********************************************************************************");
        error!(
            "At {} while processing {:?}",
            self.time, self.current_agent_for_debugging
        );
        if let Ok(path) = self.find_previous_savestate(self.time) {
            error!("Debug from {}", path);
        }
    }

    pub fn step(&mut self, map: &Map) -> Vec<Event> {
        // If there's an error, panic, so editor or headless will catch it, call dump_before_abort,
        // and also do any other bail-out handling.
        self.inner_step(map).unwrap()
    }

    fn inner_step(&mut self, map: &Map) -> Result<(Vec<Event>), Error> {
        let mut view = WorldView::new();
        let mut events: Vec<Event> = Vec::new();

        self.spawner
            .step(self.time, map, &mut self.scheduler, &mut self.parking_state);
        self.scheduler.step(
            &mut events,
            self.time,
            map,
            &mut self.parking_state,
            &mut self.walking_state,
            &mut self.driving_state,
            &mut self.trips_state,
        );

        let (newly_parked, at_border, done_biking) = self.driving_state.step(
            &mut view,
            &mut events,
            self.time,
            map,
            &self.parking_state,
            &mut self.intersection_state,
            &mut self.transit_state,
            &mut self.rng,
            &mut self.current_agent_for_debugging,
        )?;
        for p in newly_parked {
            events.push(Event::CarReachedParkingSpot(p.car, p.spot));
            capture_backtrace("CarReachedParkingSpot");
            self.parking_state.add_parked_car(p.clone());
            self.spawner.car_reached_parking_spot(
                self.time,
                p,
                map,
                &self.parking_state,
                &mut self.trips_state,
            );
        }
        for c in at_border {
            self.trips_state.car_reached_border(c, self.time);
        }
        for (bike, last_pos) in done_biking {
            // TODO push an event, backtrace, etc
            self.spawner
                .bike_reached_end(self.time, bike, last_pos, map, &mut self.trips_state);
        }

        self.walking_state.populate_view(&mut view);
        let (reached_parking, ready_to_bike) = self.walking_state.step(
            &mut events,
            TIMESTEP,
            self.time,
            map,
            &mut self.intersection_state,
            &mut self.trips_state,
            &mut self.current_agent_for_debugging,
        )?;
        for (ped, spot) in reached_parking {
            events.push(Event::PedReachedParkingSpot(ped, spot));
            capture_backtrace("PedReachedParkingSpot");
            self.spawner.ped_reached_parking_spot(
                self.time,
                ped,
                spot,
                &self.parking_state,
                &mut self.trips_state,
            );
        }
        for (ped, sidewalk_pos) in ready_to_bike {
            // TODO push an event, backtrace, etc
            self.spawner
                .ped_ready_to_bike(self.time, ped, sidewalk_pos, &mut self.trips_state);
        }

        self.transit_state.step(
            self.time,
            &mut events,
            &mut self.walking_state,
            &mut self.trips_state,
            &mut self.spawner,
            map,
        );

        // Note that the intersection sees the WorldView BEFORE the updates that just happened this
        // tick.
        self.intersection_state
            .step(&mut events, self.time, map, &view);

        // Do this at the end of the step, so that tick 0 actually occurs and things can happen
        // then.
        self.time = self.time.next();

        // Collect stats for consumers to quickly... well, consume
        self.collect_stats(map);

        // Savestate? Do this AFTER incrementing the timestep. Otherwise we could repeatedly load a
        // savestate, run a step, and invalidly save over it.
        if let Some(t) = self.savestate_every {
            if self.time.is_multiple_of(t) {
                self.save();
            }
        }

        Ok(events)
    }

    pub fn is_empty(&self) -> bool {
        self.time == Tick::zero() && self.is_done()
    }

    pub fn is_done(&self) -> bool {
        self.driving_state.is_done()
            && self.walking_state.is_done()
            && self.spawner.is_done()
            && self.scheduler.is_done()
    }

    pub fn debug_ped(&self, id: PedestrianID) {
        self.walking_state.debug_ped(id);
    }

    pub fn ped_tooltip(&self, p: PedestrianID) -> Vec<String> {
        let mut lines = self.walking_state.ped_tooltip(p);
        lines.extend(self.trips_state.tooltip_lines(AgentID::Pedestrian(p)));
        lines
    }

    pub fn car_tooltip(&self, car: CarID) -> Vec<String> {
        if let Some(mut lines) = self.driving_state.tooltip_lines(car) {
            lines.extend(self.trips_state.tooltip_lines(AgentID::Car(car)));
            lines
        } else {
            self.parking_state.tooltip_lines(car)
        }
    }

    pub fn debug_car(&mut self, id: CarID) {
        self.driving_state.toggle_debug(id);
    }

    pub fn debug_intersection(&mut self, id: IntersectionID, map: &Map) {
        self.intersection_state.debug(id, map);
    }

    pub fn save(&self) -> String {
        // If we wanted to be even more reproducible, we'd encode RNG seed, version of code, etc,
        // but that's overkill right now.
        let path = format!(
            "../data/save/{}_{}/{}/{}",
            self.map_name,
            self.edits_name,
            self.run_name,
            self.time.as_filename()
        );
        abstutil::write_json(&path, &self).expect("Writing sim state failed");
        info!("Saved to {}", path);
        path
    }

    // Earliest one is first
    fn find_all_savestates(&self) -> Result<Vec<(Tick, String)>, std::io::Error> {
        let mut results: Vec<(Tick, String)> = Vec::new();
        for entry in std::fs::read_dir(format!(
            "../data/save/{}_{}/{}/",
            self.map_name, self.edits_name, self.run_name
        ))? {
            let path = entry?.path();
            let filename = path
                .file_name()
                .expect("path isn't a filename")
                .to_os_string()
                .into_string()
                .expect("can't convert path to string");
            let tick =
                Tick::parse_filename(&filename).expect(&format!("invalid Tick: {}", filename));
            results.push((
                tick,
                path.as_path()
                    .as_os_str()
                    .to_os_string()
                    .into_string()
                    .expect("can't convert path to string"),
            ));
        }
        results.sort();
        Ok(results)
    }

    pub fn find_previous_savestate(&self, base_time: Tick) -> Result<String, std::io::Error> {
        let mut list = self.find_all_savestates()?;
        // Find the most recent one BEFORE the current time
        list.reverse();
        for (tick, path) in list {
            if tick < base_time {
                return Ok(path);
            }
        }
        Err(io_error(&format!("no savestate before {}", base_time)))
    }

    pub fn find_next_savestate(&self, base_time: Tick) -> Result<String, std::io::Error> {
        let list = self.find_all_savestates()?;
        for (tick, path) in list {
            if tick > base_time {
                return Ok(path);
            }
        }
        Err(io_error(&format!("no savestate after {}", base_time)))
    }

    pub fn active_agents(&self) -> Vec<AgentID> {
        self.trips_state.active_agents()
    }

    pub fn trace_route(&self, id: AgentID, map: &Map, dist_ahead: Distance) -> Option<Trace> {
        match id {
            AgentID::Car(car) => self.driving_state.trace_route(car, map, dist_ahead),
            AgentID::Pedestrian(ped) => self.walking_state.trace_route(ped, map, dist_ahead),
        }
    }

    pub fn get_path(&self, id: AgentID) -> Option<&Path> {
        match id {
            AgentID::Car(car) => self.driving_state.get_path(car),
            AgentID::Pedestrian(ped) => self.walking_state.get_path(ped),
        }
    }

    pub fn get_name(&self) -> &str {
        &self.run_name
    }

    // TODO dont toggle state in debug_car
    pub fn debug_trip(&mut self, id: TripID) {
        match self.trips_state.trip_to_agent(id) {
            Some(AgentID::Car(id)) => self.debug_car(id),
            Some(AgentID::Pedestrian(id)) => self.debug_ped(id),
            None => println!("{} doesn't exist", id),
        }
    }

    pub fn agent_to_trip(&self, id: AgentID) -> Option<TripID> {
        self.trips_state.agent_to_trip(id)
    }

    pub fn trip_to_agent(&self, id: TripID) -> Option<AgentID> {
        self.trips_state.trip_to_agent(id)
    }

    pub fn get_parked_cars_by_owner(&self, id: BuildingID) -> Vec<&ParkedCar> {
        self.parking_state.get_parked_cars_by_owner(id)
    }

    pub fn get_owner_of_car(&self, id: CarID) -> Option<BuildingID> {
        self.driving_state
            .get_owner_of_car(id)
            .or_else(|| self.parking_state.get_owner_of_car(id))
    }

    pub fn get_accepted_agents(&self, id: IntersectionID) -> HashSet<AgentID> {
        self.intersection_state.get_accepted_agents(id)
    }

    pub fn is_in_overtime(&self, id: IntersectionID) -> bool {
        self.intersection_state.is_in_overtime(id)
    }

    pub fn get_stats(&self) -> &SimStats {
        &self.stats
    }

    fn collect_stats(&mut self, map: &Map) {
        self.stats = SimStats::new(self.time);
        for trip in self.trips_state.get_active_trips().into_iter() {
            let pt = match self.trips_state.trip_to_agent(trip) {
                Some(AgentID::Car(id)) => self
                    .driving_state
                    .get_draw_car(id, self.time, map)
                    .map(|c| c.body.last_pt()),
                Some(AgentID::Pedestrian(id)) => self
                    .walking_state
                    .get_draw_ped(id, map, self.time)
                    .map(|p| p.pos),
                None => None,
            };
            if let Some(pt) = pt {
                self.stats.canonical_pt_per_trip.insert(trip, pt);
            }
        }
    }
}

fn io_error(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg)
}
