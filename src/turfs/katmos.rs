//Monstermos, but zoned, and multithreaded!

use super::*;

use std::{
	cell::Cell,
	{
		collections::{BTreeSet, HashMap, HashSet},
		sync::atomic::AtomicUsize,
	},
};

use indexmap::{IndexMap, IndexSet};

use ahash::RandomState;
use fxhash::FxBuildHasher;

use crate::callbacks::process_aux_callbacks;

use auxcallback::byond_callback_sender;

type TransferInfo = [f32; 7];

type MixWithID = (TurfID, TurfMixture);

type RefMixWithID<'a> = (&'a TurfID, &'a TurfMixture);

#[derive(Copy, Clone)]
struct MonstermosInfo {
	transfer_dirs: TransferInfo,
	mole_delta: f32,
	curr_transfer_amount: f32,
	curr_transfer_dir: usize,
	fast_done: bool,
}

impl Default for MonstermosInfo {
	fn default() -> MonstermosInfo {
		MonstermosInfo {
			transfer_dirs: [0_f32; 7],
			mole_delta: 0_f32,
			curr_transfer_amount: 0_f32,
			curr_transfer_dir: 6,
			fast_done: false,
		}
	}
}

#[derive(Copy, Clone)]
struct ReducedInfo {
	curr_transfer_amount: f32,
	curr_transfer_dir: usize,
}

impl Default for ReducedInfo {
	fn default() -> ReducedInfo {
		ReducedInfo {
			curr_transfer_amount: 0_f32,
			curr_transfer_dir: 6,
		}
	}
}

const OPP_DIR_INDEX: [usize; 7] = [1, 0, 3, 2, 5, 4, 6];

impl MonstermosInfo {
	fn adjust_eq_movement(&mut self, adjacent: Option<&mut Self>, dir_index: usize, amount: f32) {
		self.transfer_dirs[dir_index] += amount;
		if let Some(adj) = adjacent {
			if dir_index != 6 {
				adj.transfer_dirs[OPP_DIR_INDEX[dir_index]] -= amount;
			}
		}
	}
}

//so basically the old method of getting adjacent tiles includes the orig tile itself, don't want that here
#[derive(Clone, Copy)]
struct AdjacentTileIDsNoorig {
	adj: u8,
	i: TurfID,
	max_x: i32,
	max_y: i32,
	count: u8,
}

impl Iterator for AdjacentTileIDsNoorig {
	type Item = (u8, TurfID);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if self.count > 5 {
				return None;
			}
			self.count += 1;
			let bit = 1 << (self.count - 1);
			if self.adj & bit == bit {
				return Some((
					self.count - 1,
					adjacent_tile_id(self.count - 1, self.i, self.max_x, self.max_y),
				));
			}
		}
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		(0, Some(self.adj.count_ones() as usize))
	}
}

impl FusedIterator for AdjacentTileIDsNoorig {}

fn adjacent_tile_ids_no_orig(adj: u8, i: TurfID, max_x: i32, max_y: i32) -> AdjacentTileIDsNoorig {
	AdjacentTileIDsNoorig {
		adj,
		i,
		max_x,
		max_y,
		count: 0,
	}
}

fn finalize_eq(
	i: TurfID,
	turf: &TurfMixture,
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	let sender = byond_callback_sender();
	let transfer_dirs = {
		let maybe_monstermos_orig = info.get_mut(&i);
		if maybe_monstermos_orig.is_none() {
			return;
		}
		let monstermos_orig = maybe_monstermos_orig.unwrap();
		let transfer_dirs = monstermos_orig.transfer_dirs;
		monstermos_orig
			.transfer_dirs
			.iter_mut()
			.for_each(|a| *a = 0.0); // null it out to prevent infinite recursion.
		transfer_dirs
	};
	let planet_transfer_amount = transfer_dirs[6];
	if planet_transfer_amount > 0.0 {
		if turf.total_moles() < planet_transfer_amount {
			finalize_eq_neighbors(i, turf, turfs, transfer_dirs, max_x, max_y, info);
		}
		drop(GasArena::with_gas_mixture_mut(turf.mix, |gas| {
			gas.add(-planet_transfer_amount);
			Ok(())
		}));
	} else if planet_transfer_amount < 0.0 {
		if let Some(air_entry) = turf
			.planetary_atmos
			.and_then(|i| planetary_atmos().try_get(&i).try_unwrap())
		{
			let planet_air = air_entry.value();
			let planet_sum = planet_air.total_moles();
			if planet_sum > 0.0 {
				drop(GasArena::with_gas_mixture_mut(turf.mix, |gas| {
					gas.merge(&(planet_air * (-planet_transfer_amount / planet_sum)));
					Ok(())
				}));
			}
		}
	}
	for (j, adj_id) in adjacent_tile_ids_no_orig(turf.adjacency, i, max_x, max_y) {
		let amount = transfer_dirs[j as usize];
		if amount > 0.0 {
			if turf.total_moles() < amount {
				finalize_eq_neighbors(i, turf, turfs, transfer_dirs, max_x, max_y, info);
			}
			if let Some(mut adj_info) = info.get_mut(&adj_id) {
				if let Some(adj_turf) = turfs.get(&adj_id) {
					adj_info.transfer_dirs[OPP_DIR_INDEX[j as usize]] = 0.0;
					if turf.mix != adj_turf.mix {
						drop(GasArena::with_gas_mixtures_mut(
							turf.mix,
							adj_turf.mix,
							|air, other_air| {
								other_air.merge(&air.remove(amount));
								Ok(())
							},
						));
					}
					drop(sender.try_send(Box::new(move || {
						let real_amount = Value::from(amount);
						let turf = unsafe { Value::turf_by_id_unchecked(i as u32) };
						let other_turf = unsafe { Value::turf_by_id_unchecked(adj_id as u32) };
						if let Err(e) =
							turf.call("consider_pressure_difference", &[&other_turf, &real_amount])
						{
							Proc::find(byond_string!("/proc/stack_trace"))
								.ok_or_else(|| runtime!("Couldn't find stack_trace!"))?
								.call(&[&Value::from_string(e.message.as_str())?])?;
						}
						Ok(())
					})));
				}
			}
		}
	}
}

fn finalize_eq_neighbors(
	i: TurfID,
	turf: &TurfMixture,
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	transfer_dirs: [f32; 7],
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	for (j, adjacent_id) in adjacent_tile_ids_no_orig(turf.adjacency, i, max_x, max_y) {
		let amount = transfer_dirs[j as usize];
		if amount < 0.0 {
			let other_turf = {
				let maybe = turfs.get(&adjacent_id);
				if maybe.is_none() {
					continue;
				}
				maybe.unwrap()
			};
			finalize_eq(adjacent_id, other_turf, turfs, max_x, max_y, info);
		}
	}
}

fn monstermos_fast_process(
	i: TurfID,
	m: &TurfMixture,
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	let mut cur_info = {
		let maybe_cur_orig = info.get_mut(&i);
		if maybe_cur_orig.is_none() {
			return;
		}
		let mut cur_info = maybe_cur_orig.unwrap();
		cur_info.fast_done = true;
		*cur_info
	};
	let mut eligible_adjacents: u8 = 0;
	if cur_info.mole_delta > 0.0 {
		for (j, loc) in adjacent_tile_ids_no_orig(m.adjacency, i, max_x, max_y) {
			if turfs.get(&loc).map_or(false, TurfMixture::enabled) {
				if let Some(adj_info) = info.get(&loc) {
					if !adj_info.fast_done {
						eligible_adjacents |= 1 << j;
					}
				}
			}
		}
		let amt_eligible = eligible_adjacents.count_ones();
		if amt_eligible == 0 {
			info.entry(i).and_modify(|entry| *entry = cur_info);
			return;
		}
		let moles_to_move = cur_info.mole_delta / amt_eligible as f32;
		for (j, loc) in adjacent_tile_ids_no_orig(eligible_adjacents, i, max_x, max_y) {
			if let Some(mut adj_info) = info.get_mut(&loc) {
				cur_info.adjust_eq_movement(Some(&mut adj_info), j as usize, moles_to_move);
				cur_info.mole_delta -= moles_to_move;
				adj_info.mole_delta += moles_to_move;
			}
			info.entry(i).and_modify(|entry| *entry = cur_info);
		}
	}
}

fn give_to_takers(
	giver_turfs: &[RefMixWithID],
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	let mut queue: IndexMap<TurfID, &TurfMixture, FxBuildHasher> =
		IndexMap::with_hasher(FxBuildHasher::default());

	for &(i, m) in giver_turfs {
		let mut giver_info = {
			let maybe_giver_orig = info.get_mut(i);
			if maybe_giver_orig.is_none() {
				continue;
			}
			let mut giver_info = maybe_giver_orig.unwrap();
			giver_info.curr_transfer_dir = 6;
			giver_info.curr_transfer_amount = 0.0;
			*giver_info
		};
		queue.insert(*i, m);
		let mut queue_idx = 0;
		while let Some((idx, turf)) = queue.get_index(queue_idx) {
			if giver_info.mole_delta <= 0.0 {
				break;
			}
			for (j, loc) in adjacent_tile_ids_no_orig(turf.adjacency, *idx, max_x, max_y) {
				if giver_info.mole_delta <= 0.0 {
					break;
				}
				if let Some(mut adj_info) = info.get_mut(&loc) {
					if let Some(adj_mix) = turfs
						.get(&loc)
						.and_then(|terf| terf.enabled().then(|| terf))
					{
						if queue.insert(loc, adj_mix).is_none() {
							adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
							adj_info.curr_transfer_amount = 0.0;
							if adj_info.mole_delta < 0.0 {
								// this turf needs gas. Let's give it to 'em.
								if -adj_info.mole_delta > giver_info.mole_delta {
									// we don't have enough gas
									adj_info.curr_transfer_amount -= giver_info.mole_delta;
									adj_info.mole_delta += giver_info.mole_delta;
									giver_info.mole_delta = 0.0;
								} else {
									// we have enough gas.
									adj_info.curr_transfer_amount += adj_info.mole_delta;
									giver_info.mole_delta += adj_info.mole_delta;
									adj_info.mole_delta = 0.0;
								}
							}
						}
					}
				}
				info.entry(*i).and_modify(|entry| *entry = giver_info);
			}
			queue_idx += 1;
		}
		for (idx, _) in queue.drain(..).rev() {
			let mut turf_info = {
				let opt = info.get(&idx);
				if opt.is_none() {
					continue;
				}
				*opt.unwrap()
			};
			if turf_info.curr_transfer_amount != 0.0 && turf_info.curr_transfer_dir != 6 {
				if let Some(mut adj_info) = info.get_mut(&adjacent_tile_id(
					turf_info.curr_transfer_dir as u8,
					idx,
					max_x,
					max_y,
				)) {
					let (dir, amt) = (turf_info.curr_transfer_dir, turf_info.curr_transfer_amount);
					turf_info.adjust_eq_movement(Some(&mut adj_info), dir, amt);
					adj_info.curr_transfer_amount += turf_info.curr_transfer_amount;
					turf_info.curr_transfer_amount = 0.0;
				}
			}
			info.entry(idx).and_modify(|cur_info| *cur_info = turf_info);
		}
	}
}

fn take_from_givers(
	taker_turfs: &[RefMixWithID],
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	let mut queue: IndexMap<TurfID, &TurfMixture, FxBuildHasher> =
		IndexMap::with_hasher(FxBuildHasher::default());

	for &(i, m) in taker_turfs {
		let mut taker_info = {
			let maybe_taker_orig = info.get_mut(i);
			if maybe_taker_orig.is_none() {
				continue;
			}
			let mut taker_info = maybe_taker_orig.unwrap();
			taker_info.curr_transfer_dir = 6;
			taker_info.curr_transfer_amount = 0.0;
			*taker_info
		};
		queue.insert(*i, m);
		let mut queue_idx = 0;
		while let Some((idx, turf)) = queue.get_index(queue_idx) {
			if taker_info.mole_delta >= 0.0 {
				break;
			}
			for (j, loc) in adjacent_tile_ids_no_orig(turf.adjacency, *idx, max_x, max_y) {
				if taker_info.mole_delta >= 0.0 {
					break;
				}
				if let Some(mut adj_info) = info.get_mut(&loc) {
					if let Some(adj_mix) = turfs
						.get(&loc)
						.and_then(|terf| terf.enabled().then(|| terf))
					{
						if queue.insert(loc, adj_mix).is_none() {
							adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
							adj_info.curr_transfer_amount = 0.0;
							if adj_info.mole_delta > 0.0 {
								// this turf has gas we can succ. Time to succ.
								if adj_info.mole_delta > -taker_info.mole_delta {
									// they have enough gase
									adj_info.curr_transfer_amount -= taker_info.mole_delta;
									adj_info.mole_delta += taker_info.mole_delta;
									taker_info.mole_delta = 0.0;
								} else {
									// they don't have neough gas
									adj_info.curr_transfer_amount += adj_info.mole_delta;
									taker_info.mole_delta += adj_info.mole_delta;
									adj_info.mole_delta = 0.0;
								}
							}
						}
					}
				}
				info.entry(*i).and_modify(|entry| *entry = taker_info);
			}
			queue_idx += 1;
		}
		for (idx, _) in queue.drain(..).rev() {
			let mut turf_info = {
				let opt = info.get(&idx);
				if opt.is_none() {
					continue;
				}
				*opt.unwrap()
			};
			if turf_info.curr_transfer_amount != 0.0 && turf_info.curr_transfer_dir != 6 {
				if let Some(mut adj_info) = info.get_mut(&adjacent_tile_id(
					turf_info.curr_transfer_dir as u8,
					idx,
					max_x,
					max_y,
				)) {
					let (dir, amt) = (turf_info.curr_transfer_dir, turf_info.curr_transfer_amount);
					turf_info.adjust_eq_movement(Some(&mut adj_info), dir, amt);
					adj_info.curr_transfer_amount += turf_info.curr_transfer_amount;
					turf_info.curr_transfer_amount = 0.0;
				}
			}
			info.entry(idx).and_modify(|cur_info| *cur_info = turf_info);
		}
	}
}

fn explosively_depressurize(
	turf_idx: TurfID,
	max_x: i32,
	max_y: i32,
	equalize_hard_turf_limit: usize,
) -> Result<(), Runtime> {
	let mut info: HashMap<TurfID, Cell<ReducedInfo>, FxBuildHasher> =
		HashMap::with_hasher(FxBuildHasher::default());
	let mut turfs: IndexSet<TurfID, FxBuildHasher> =
		IndexSet::with_hasher(FxBuildHasher::default());
	let mut progression_order: IndexSet<MixWithID, RandomState> =
		IndexSet::with_hasher(RandomState::default());
	let mut space_turfs: IndexSet<TurfID, FxBuildHasher> =
		IndexSet::with_hasher(FxBuildHasher::default());
	turfs.insert(turf_idx);
	let mut warned_about_planet_atmos = false;
	let mut cur_queue_idx = 0;
	while cur_queue_idx < turfs.len() {
		let i = turfs[cur_queue_idx];
		cur_queue_idx += 1;
		let m = {
			let maybe = turf_gases().get(&i);
			if maybe.is_none() {
				continue;
			}
			*maybe.unwrap()
		};
		if m.planetary_atmos.is_some() {
			warned_about_planet_atmos = true;
			continue;
		}
		if m.is_immutable() {
			if space_turfs.insert(i) {
				unsafe { Value::turf_by_id_unchecked(i) }
					.set(byond_string!("pressure_specific_target"), &unsafe {
						Value::turf_by_id_unchecked(i)
					})?;
			}
		} else {
			if cur_queue_idx > equalize_hard_turf_limit {
				continue;
			}
			for (_, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
				let insert_success = {
					if turf_gases().get(&loc).is_some() {
						turfs.insert(loc)
					} else {
						false
					}
				};
				if insert_success {
					unsafe { Value::turf_by_id_unchecked(i) }.call(
						"consider_firelocks",
						&[&unsafe { Value::turf_by_id_unchecked(loc) }],
					)?;
				}
			}
		}
		if warned_about_planet_atmos {
			return Ok(()); // planet atmos > space
		}
	}

	process_aux_callbacks(crate::callbacks::TURFS);
	process_aux_callbacks(crate::callbacks::ADJACENCIES);

	if space_turfs.is_empty() {
		return Ok(());
	}

	for i in space_turfs.iter() {
		let maybe_turf = turf_gases().get(i);
		if maybe_turf.is_none() {
			continue;
		}
		let m = *maybe_turf.unwrap();
		progression_order.insert((*i, m));
	}

	cur_queue_idx = 0;
	while cur_queue_idx < progression_order.len() {
		let (i, m) = progression_order[cur_queue_idx];
		cur_queue_idx += 1;
		if cur_queue_idx > equalize_hard_turf_limit {
			continue;
		}
		for (j, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
			if let Some(adj_m) = { turf_gases().get(&loc) } {
				let adj_orig = info.entry(loc).or_default();
				let mut adj_info = adj_orig.get();
				if !adj_m.is_immutable() && progression_order.insert((loc, *adj_m)) {
					adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
					adj_info.curr_transfer_amount = 0.0;
					let cur_target_turf = unsafe { Value::turf_by_id_unchecked(i) }
						.get(byond_string!("pressure_specific_target"))?;
					unsafe { Value::turf_by_id_unchecked(loc) }
						.set(byond_string!("pressure_specific_target"), &cur_target_turf)?;
					adj_orig.set(adj_info);
				}
			}
		}
	}
	let hpd = auxtools::Value::globals()
		.get(byond_string!("SSair"))?
		.get_list(byond_string!("high_pressure_delta"))
		.map_err(|_| {
			runtime!(
				"Attempt to interpret non-list value as list {} {}:{}",
				std::file!(),
				std::line!(),
				std::column!()
			)
		})?;
	for (i, m) in progression_order.iter().rev() {
		let cur_orig = info.entry(*i).or_default();
		let mut cur_info = cur_orig.get();
		if cur_info.curr_transfer_dir == 6 {
			continue;
		}
		let mut in_hpd = false;
		for k in 1..=hpd.len() {
			if let Ok(hpd_val) = hpd.get(k) {
				if hpd_val == unsafe { Value::turf_by_id_unchecked(*i) } {
					in_hpd = true;
					break;
				}
			}
		}
		if !in_hpd {
			hpd.append(&unsafe { Value::turf_by_id_unchecked(*i) });
		}
		let loc = adjacent_tile_id(cur_info.curr_transfer_dir as u8, *i, max_x, max_y);
		let mut sum = 0.0_f32;

		if let Some(adj_m) = turf_gases().get(&loc) {
			sum = adj_m.total_moles();
		};

		cur_info.curr_transfer_amount += sum;
		cur_orig.set(cur_info);

		let adj_orig = info.entry(loc).or_default();
		let mut adj_info = adj_orig.get();

		adj_info.curr_transfer_amount += cur_info.curr_transfer_amount;
		adj_orig.set(adj_info);

		let byond_turf = unsafe { Value::turf_by_id_unchecked(*i) };

		byond_turf.set(
			byond_string!("pressure_difference"),
			Value::from(cur_info.curr_transfer_amount),
		)?;
		byond_turf.set(
			byond_string!("pressure_direction"),
			Value::from((1 << cur_info.curr_transfer_dir) as f32),
		)?;

		if adj_info.curr_transfer_dir == 6 {
			let byond_turf_adj = unsafe { Value::turf_by_id_unchecked(loc) };
			byond_turf_adj.set(
				byond_string!("pressure_difference"),
				Value::from(adj_info.curr_transfer_amount),
			)?;
			byond_turf_adj.set(
				byond_string!("pressure_direction"),
				Value::from((1 << cur_info.curr_transfer_dir) as f32),
			)?;
		}
		m.clear_air();
		byond_turf.call("handle_decompression_floor_rip", &[&Value::from(sum)])?;
	}
	Ok(())
	//	if (total_gases_deleted / turfs.len() as f32) > 20.0 && turfs.len() > 10 { // logging I guess
	//	}
}

// Clippy go away, this type is only used once
#[allow(clippy::type_complexity)]
fn flood_fill_equalize_turfs(
	i: TurfID,
	m: TurfMixture,
	max_x: i32,
	max_y: i32,
	equalize_hard_turf_limit: usize,
	found_turfs: &mut HashSet<TurfID, FxBuildHasher>,
) -> Option<(
	IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	f64,
)> {
	let mut turfs: IndexMap<TurfID, TurfMixture, FxBuildHasher> =
		IndexMap::with_hasher(FxBuildHasher::default());
	let mut border_turfs: std::collections::VecDeque<MixWithID> = std::collections::VecDeque::new();
	let mut planet_turfs: IndexMap<TurfID, TurfMixture, FxBuildHasher> =
		IndexMap::with_hasher(FxBuildHasher::default());
	let sender = byond_callback_sender();
	let mut total_moles = 0.0_f64;
	border_turfs.push_back((i, m));
	found_turfs.insert(i);
	let mut ignore_zone = false;
	while let Some((cur_idx, cur_turf)) = border_turfs.pop_front() {
		if cur_turf.planetary_atmos.is_some() {
			planet_turfs.insert(cur_idx, cur_turf);
			continue;
		}
		total_moles += cur_turf.total_moles() as f64;
		for (_, loc) in adjacent_tile_ids(cur_turf.adjacency, cur_idx, max_x, max_y) {
			if found_turfs.insert(loc) {
				let result = turf_gases().try_get(&loc);
				if result.is_locked() {
					ignore_zone = true;
					continue;
				}
				if let Some(adj_turf) = result.try_unwrap() {
					if adj_turf.enabled() {
						border_turfs.push_back((loc, *adj_turf.value()));
					}
					if adj_turf.value().is_immutable() {
						// Uh oh! looks like someone opened an airlock to space! TIME TO SUCK ALL THE AIR OUT!!!
						// NOT ONE OF YOU IS GONNA SURVIVE THIS
						// (I just made explosions less laggy, you're welcome)
						if !ignore_zone {
							drop(sender.try_send(Box::new(move || {
								explosively_depressurize(i, max_x, max_y, equalize_hard_turf_limit)
							})));
						}
						ignore_zone = true;
					}
				}
			}
		}
		turfs.insert(cur_idx, cur_turf);
	}
	(!ignore_zone).then(|| (turfs, planet_turfs, total_moles))
}

fn process_planet_turfs(
	planet_turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	turfs: &IndexMap<TurfID, TurfMixture, FxBuildHasher>,
	average_moles: f32,
	max_x: i32,
	max_y: i32,
	equalize_hard_turf_limit: usize,
	info: &mut HashMap<TurfID, MonstermosInfo, FxBuildHasher>,
) {
	let sender = byond_callback_sender();
	let sample_turf = planet_turfs[0];
	let sample_planet_atmos = sample_turf.planetary_atmos;
	if sample_planet_atmos.is_none() {
		return;
	}
	let maybe_planet_sum = planetary_atmos()
		.try_get(&sample_planet_atmos.unwrap())
		.try_unwrap();
	if maybe_planet_sum.is_none() {
		return;
	}
	let planet_sum = maybe_planet_sum.unwrap().value().total_moles();
	let target_delta = planet_sum - average_moles;

	let mut progression_order: IndexSet<TurfID, FxBuildHasher> =
		IndexSet::with_hasher(FxBuildHasher::default());

	for (i, _) in planet_turfs.iter() {
		progression_order.insert(*i);
		let mut cur_info = info.entry(*i).or_default();
		cur_info.curr_transfer_dir = 6;
	}
	// now build a map of where the path to a planet turf is for each tile.
	let mut queue_idx = 0;
	while queue_idx < progression_order.len() {
		let i = progression_order[queue_idx];
		queue_idx += 1;
		let maybe_m = turfs.get(&i);
		if maybe_m.is_none() {
			info.entry(i)
				.and_modify(|entry| *entry = MonstermosInfo::default());
			continue;
		}
		let m = *maybe_m.unwrap();
		for (j, loc) in adjacent_tile_ids_no_orig(m.adjacency, i, max_x, max_y) {
			if let Some(mut adj_info) = info.get_mut(&loc) {
				if queue_idx < equalize_hard_turf_limit {
					drop(sender.try_send(Box::new(move || {
						let that_turf = unsafe { Value::turf_by_id_unchecked(loc) };
						let this_turf = unsafe { Value::turf_by_id_unchecked(i) };
						this_turf.call("consider_firelocks", &[&that_turf])?;
						Ok(())
					})));
				}
				if let Some(adj) = turfs
					.get(&loc)
					.and_then(|terf| terf.enabled().then(|| terf))
				{
					if !progression_order.insert(loc) || adj.planetary_atmos.is_some() {
						continue;
					}
					adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
				}
			}
		}
	}
	for i in progression_order.iter().rev() {
		if turfs.get(i).is_none() {
			continue;
		}
		let mut cur_info = {
			if let Some(opt) = info.get(&i) {
				*opt
			} else {
				continue;
			}
		};
		let airflow = cur_info.mole_delta - target_delta;
		let dir = cur_info.curr_transfer_dir;
		if cur_info.curr_transfer_dir == 6 {
			cur_info.adjust_eq_movement(None, dir, airflow);
			cur_info.mole_delta = target_delta;
		} else if let Some(mut adj_info) = info.get_mut(&adjacent_tile_id(
			cur_info.curr_transfer_dir as u8,
			*i,
			max_x,
			max_y,
		)) {
			cur_info.adjust_eq_movement(Some(&mut adj_info), dir, airflow);
			adj_info.mole_delta += airflow;
			cur_info.mole_delta = target_delta;
		}
		info.entry(*i).and_modify(|info| *info = cur_info);
	}
}

pub(crate) fn equalize(
	max_x: i32,
	max_y: i32,
	equalize_hard_turf_limit: usize,
	high_pressure_turfs: &BTreeSet<TurfID>,
) -> usize {
	let turfs_processed: AtomicUsize = AtomicUsize::new(0);
	let mut found_turfs: HashSet<TurfID, FxBuildHasher> =
		HashSet::with_hasher(FxBuildHasher::default());
	let zoned_turfs = high_pressure_turfs
		.iter()
		.filter_map(|i| {
			if found_turfs.contains(i) {
				return None;
			};
			let m = *turf_gases().try_get(i).try_unwrap()?;
			if !m.enabled()
				|| m.adjacency == 0
				|| GasArena::with_all_mixtures(|all_mixtures| {
					let our_moles = all_mixtures[m.mix].read().total_moles();
					our_moles < 10.0
						|| m.adjacent_mixes(all_mixtures).all(|lock| {
							(lock.read().total_moles() - our_moles).abs()
								< MINIMUM_MOLES_DELTA_TO_MOVE
						})
				}) {
				return None;
			}
			flood_fill_equalize_turfs(
				*i,
				m,
				max_x,
				max_y,
				equalize_hard_turf_limit,
				&mut found_turfs,
			)
		})
		.collect::<Vec<_>>();

	let turfs = zoned_turfs
		.into_par_iter()
		.map(|(turfs, planet_turfs, total_moles)| {
			let average_moles = (total_moles / (turfs.len() - planet_turfs.len()) as f64) as f32;

			let mut info = turfs
				.par_iter()
				.map(|(&index, mixture)| {
					let mut cur_info = MonstermosInfo::default();
					cur_info.mole_delta = mixture.total_moles() - average_moles;
					(index, cur_info)
				})
				.collect::<HashMap<_, _, FxBuildHasher>>();

			let (mut giver_turfs, mut taker_turfs): (Vec<_>, Vec<_>) = turfs
				.iter()
				.filter(|(_, cur_mixture)| cur_mixture.planetary_atmos.is_none())
				.partition(|(i, _)| info.get(i).unwrap().mole_delta > 0.0);

			let log_n = ((turfs.len() as f32).log2().floor()) as usize;
			if giver_turfs.len() > log_n && taker_turfs.len() > log_n {
				for (&i, m) in &turfs {
					monstermos_fast_process(i, m, &turfs, max_x, max_y, &mut info);
				}
				giver_turfs.clear();
				taker_turfs.clear();

				giver_turfs.extend(turfs.iter().filter(|&(i, m)| {
					info.entry(*i).or_default().mole_delta > 0.0 && m.planetary_atmos.is_none()
				}));

				taker_turfs.extend(turfs.iter().filter(|&(i, m)| {
					info.entry(*i).or_default().mole_delta <= 0.0 && m.planetary_atmos.is_none()
				}));
			}

			// alright this is the part that can become O(n^2).
			if giver_turfs.len() < taker_turfs.len() {
				// as an optimization, we choose one of two methods based on which list is smaller.
				give_to_takers(&giver_turfs, &turfs, max_x, max_y, &mut info);
			} else {
				take_from_givers(&taker_turfs, &turfs, max_x, max_y, &mut info);
			}
			if planet_turfs.is_empty() {
				turfs_processed.fetch_add(turfs.len(), std::sync::atomic::Ordering::Relaxed);
			} else {
				turfs_processed.fetch_add(
					turfs.len() + planet_turfs.len(),
					std::sync::atomic::Ordering::Relaxed,
				);
				process_planet_turfs(
					&planet_turfs,
					&turfs,
					average_moles,
					max_x,
					max_y,
					equalize_hard_turf_limit,
					&mut info,
				);
			}
			(turfs, info)
		})
		.collect::<Vec<_>>();

	turfs.into_par_iter().for_each(|(turf, mut info)| {
		turf.iter().for_each(|(i, m)| {
			finalize_eq(*i, m, &turf, max_x, max_y, &mut info);
		});
	});
	turfs_processed.load(std::sync::atomic::Ordering::Relaxed)
}
