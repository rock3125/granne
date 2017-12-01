//
// concurrent, little waiting (X)
// mmap (X)
// build layer by layer (X)
// extenstible (X)
// small size (X)
// fast
// merge indexes?
//

use arrayvec::ArrayVec;
use fnv::FnvHashSet;
use ordered_float::NotNaN;
use revord::RevOrd;
use std::collections::BinaryHeap;
use std::cmp;
use types::*;
use pbr::ProgressBar;

// Write and read
use std::fs::File;
use std::io::{Write, Result};

// Threading
use rayon::prelude::*;
use std::sync::{Mutex, RwLock};

const MAX_NEIGHBORS: usize = 32;
type NeighborType = u32;

#[repr(C)]
#[derive(Clone, Default, Debug)]
struct HnswNode {
    neighbors: ArrayVec<[NeighborType; MAX_NEIGHBORS]>,
}


pub struct Config {
    pub num_levels: usize,
    pub level_multiplier: usize,
    pub max_search: usize,
    pub show_progress: bool,
}


pub struct HnswBuilder<'a, T: HasDistance + Sync + Send + 'a> {
    levels: Vec<Vec<HnswNode>>,
    elements: &'a [T],
    config: Config,
}


pub struct Hnsw<'a, T: HasDistance + 'a> {
    levels: Vec<&'a [HnswNode]>,
    elements: &'a [T],
}


impl<'a, T: HasDistance + Sync + Send + 'a> HnswBuilder<'a, T> {

    pub fn new(config: Config, elements: &'a [T]) -> Self {
        assert!(elements.len() < <NeighborType>::max_value() as usize);

        HnswBuilder {
            levels: Vec::new(),
            elements: elements,
            config: config,
        }
    }


    pub fn save_to_disk(self: &Self, path: &str) {

        let mut file = File::create(path).unwrap();

        self.write(&mut file);
    }


    pub fn write<B: Write>(self: &Self, buffer: &mut B) -> Result<()> {
        let num_nodes = self.levels.iter().map(|level| level.len()).sum();
        let num_levels = self.levels.len();
        let level_counts = self.levels.iter().map(|level| level.len());

        let mut usize_data = vec![num_nodes, num_levels];
        usize_data.extend(level_counts);

        let data = unsafe {
            ::std::slice::from_raw_parts(
                usize_data.as_ptr() as *const u8,
                usize_data.len() * ::std::mem::size_of::<usize>())
        };

        buffer.write(data)?;

        for level in &self.levels {

            let data = unsafe {
                ::std::slice::from_raw_parts(
                    level.as_ptr() as *const u8,
                    level.len() * ::std::mem::size_of::<HnswNode>())
            };

            buffer.write(data)?;
        }

        Ok(())
    }


    pub fn get_index(self: &'a Self) -> Hnsw<'a, T> {
        Hnsw {
            levels: self.levels
                .iter()
                .map(|level| &level[..])
                .collect(),
            elements: self.elements,
        }
    }


    pub fn load(config: Config, index: &Hnsw<T>, elements: &'a [T]) -> Self {
        let mut builder = Self::new(config, elements);

        assert!(index.levels.last().unwrap().len() <= elements.len());

        builder.levels = index.levels.iter()
            .map(|level| level.to_vec())
            .collect();

        builder
    }


    pub fn build_index(&mut self) {
        self.levels.push(vec![HnswNode::default()]);

        let mut num_elements = 1;
        for _level in 1..self.config.num_levels {
            num_elements *= self.config.level_multiplier;
            num_elements = cmp::min(num_elements, self.elements.len());

            // copy layer above
            let mut new_layer = Vec::with_capacity(num_elements);
            new_layer.extend_from_slice(self.levels.last().unwrap());

            Self::insert_elements(&self.config,
                                  &mut new_layer,
                                  &self.get_index(),
                                  &self.elements[..num_elements]);

            self.levels.push(new_layer);
        }
    }


    pub fn append_elements(&mut self, elements: &'a [T]) {
        assert!(elements.len() < <NeighborType>::max_value() as usize);

        assert!(self.elements[0].dist(&elements[0]).into_inner() <
                DIST_EPSILON);

        assert!(self.elements[self.elements.len()-1].dist(
                     &elements[self.elements.len()-1]).into_inner() <
                DIST_EPSILON);

        self.elements = elements;

        let mut layer = self.levels.pop().unwrap();

        Self::insert_elements(&self.config,
                              &mut layer,
                              &self.get_index(),
                              self.elements);

        self.levels.push(layer);
    }


    fn insert_elements(config: &Config,
                       layer: &mut Vec<HnswNode>,
                       index: &Hnsw<T>,
                       elements: &[T]) {

        assert!(layer.len() <= elements.len());

        let already_inserted = layer.len();

        layer.resize(elements.len(), HnswNode::default());

        // create RwLocks for underlying nodes
        let layer: Vec<RwLock<&mut HnswNode>> =
            layer.iter_mut()
            .map(|node| RwLock::new(node))
            .collect();

        // set up progress bar
        let step_size = cmp::max(10, elements.len() / 100);
        let progress_bar = {
            if config.show_progress {
                let mut progress_bar = ProgressBar::new(elements.len() as u64);
                let info_text = format!("Level {}: ", index.levels.len());
                progress_bar.message(&info_text);
                progress_bar.set((step_size * (already_inserted / step_size)) as u64);

                Some(Mutex::new(progress_bar))

            } else {
                None
            }
        };

        // insert elements, skipping already inserted
        elements.par_iter()
            .enumerate()
            .skip(already_inserted)
            .for_each(
                |(idx, _)| {
                    Self::insert_element(config,
                                         index,
                                         &layer,
                                         elements,
                                         idx);

                    // This only shows approximate progress because of par_iter
                    if idx % step_size == 0 {
                        if let Some(ref progress_bar) = progress_bar {
                            progress_bar.lock().unwrap().add(step_size as u64);
                        }
                    }
                }
            );

        if let Some(progress_bar) = progress_bar {
            progress_bar.lock().unwrap().finish_println("");
        }
    }


    fn insert_element(config: &Config,
                      index: &Hnsw<T>,
                      layer: &Vec<RwLock<&mut HnswNode>>,
                      elements: &[T],
                      idx: usize) {

        let element = &elements[idx];
        let (entrypoint, _) = index.search(element, config.max_search / 10)[0];

        let neighbors = Self::search_for_neighbors_index(&layer[..],
                                                         entrypoint,
                                                         elements,
                                                         element,
                                                         config.max_search,
                                                         MAX_NEIGHBORS);

        let neighbors =
            Self::select_neighbors(idx,
                                   neighbors,
                                   elements,
                                   MAX_NEIGHBORS);

        Self::initialize_neighbors(&layer[idx], &neighbors[..]);

        for (neighbor, d) in neighbors {
            Self::connect_nodes(&layer[neighbor], elements, neighbor, idx, d);
        }
    }


    // Similar to Hnsw::search_for_neighbors but with RwLocks for
    // parallel insertion
    fn search_for_neighbors_index(layer: &[RwLock<&mut HnswNode>],
                                  entrypoint: usize,
                                  elements: &[T],
                                  goal: &T,
                                  max_search: usize,
                                  max_neighbors: usize)
                                  -> Vec<(usize, NotNaN<f32>)> {

        let mut res: MaxSizeHeap<(NotNaN<f32>, usize)> =
            MaxSizeHeap::new(max_search);
        let mut pq: BinaryHeap<RevOrd<_>> = BinaryHeap::new();
        let mut visited = FnvHashSet::default();

        pq.push(RevOrd(
            (elements[entrypoint].dist(&goal), entrypoint)
        ));

        visited.insert(entrypoint);

        while let Some(RevOrd((d, idx))) = pq.pop() {
            if res.is_full() && d > res.peek().unwrap().0 {
                break;
            }

            res.push((d, idx));

            let node = layer[idx].read().unwrap();

            for neighbor_idx in node.neighbors.iter().map(|&n| n as usize) {
                if visited.insert(neighbor_idx) {
                    let distance = elements[neighbor_idx].dist(&goal);

                    if !res.is_full() || distance < res.peek().unwrap().0 {
                        pq.push(RevOrd((distance, neighbor_idx)));
                    }
                }
            }
        }

        res.heap
            .into_sorted_vec()
            .into_iter().take(max_neighbors)
            .map(|(d, idx)| (idx, d))
            .collect()
    }


    fn select_neighbors(idx: usize,
                        candidates: Vec<(usize, NotNaN<f32>)>,
                        elements: &[T],
                        max_neighbors: usize) -> Vec<(usize, NotNaN<f32>)> {
        if candidates.len() <= max_neighbors {
            return candidates;
        }

        let mut res = Vec::new();
        let mut pruned = Vec::new();
        // candidates is sorted on distance from idx
        for (j, d) in candidates.into_iter() {
            if res.len() >= max_neighbors {
                break;
            }

            if res.iter().all(|k| d < elements[idx].dist(&elements[j])) {
                res.push((j, d));
            } else {
                pruned.push((j, d));
            }
        }

        let remaining = max_neighbors - res.len();
        res.extend(pruned.into_iter().take(remaining));

        res
    }



    fn initialize_neighbors(node: &RwLock<&mut HnswNode>,
                            neighbors: &[(usize, NotNaN<f32>)]) {
        // Write Lock!
        let mut node = node.write().unwrap();

        debug_assert!(node.neighbors.len() == 0);
        let num_to_add =
            node.neighbors.capacity() - node.neighbors.len();

        for &(idx, _) in neighbors.iter().take(num_to_add) {
            node.neighbors.push(idx as NeighborType);
        }
    }


    fn connect_nodes(node: &RwLock<&mut HnswNode>,
                     elements: &[T],
                     i: usize,
                     j: usize,
                     d: NotNaN<f32>)
    {
        // Write Lock!
        let mut node = node.write().unwrap();

        if node.neighbors.len() < MAX_NEIGHBORS {
            node.neighbors.push(j as NeighborType);
        } else {

            let mut candidates: Vec<_> = node.neighbors.iter()
                .map(|&k| (k as usize, elements[i].dist(&elements[k as usize])))
                .collect();

            candidates.push((j as usize, d));
            candidates.sort_unstable_by_key(|&(_, d)| d);
            let neighbors = Self::select_neighbors(i, candidates, &elements, MAX_NEIGHBORS);

            for (k, (n, _)) in neighbors.into_iter().enumerate() {
                node.neighbors[k] = n as u32;
            }
        }
    }
}


impl<'a, T: HasDistance + 'a> Hnsw<'a, T> {

    pub fn load(buffer: &'a [u8], elements: &'a [T]) -> Self {

        let offset = 0 * ::std::mem::size_of::<usize>();
        let num_nodes = &buffer[offset] as *const u8 as *const usize;

        let offset = 1 * ::std::mem::size_of::<usize>();
        let num_levels = &buffer[offset] as *const u8 as *const usize;

        let offset = 2 * ::std::mem::size_of::<usize>();

        let level_counts: &[usize] = unsafe {
            ::std::slice::from_raw_parts(
                &buffer[offset] as *const u8 as *const usize,
                *num_levels
        )};

        let offset = (2 + level_counts.len()) * ::std::mem::size_of::<usize>();

        let nodes: &[HnswNode] = unsafe {
            ::std::slice::from_raw_parts(
                &buffer[offset] as *const u8 as *const HnswNode,
                *num_nodes
            )
        };

        let mut levels = Vec::new();

        let mut start = 0;
        for &level_count in level_counts {
            let end = start + level_count;
            let level = &nodes[start..end];
            levels.push(level);
            start = end;
        }

        assert!(levels.last().unwrap().len() <= elements.len());

        Self {
            levels: levels,
            elements: elements,
        }
    }


    pub fn search(&self, element: &T, max_search: usize) -> Vec<(usize, f32)> {

        let (bottom_level, top_levels) = self.levels.split_last().unwrap();

        let entrypoint = Self::find_entrypoint(&top_levels,
                                               element,
                                               &self.elements,
                                               cmp::max(50, max_search / 50));

        Self::search_for_neighbors(
            &bottom_level,
            entrypoint,
            &self.elements,
            element,
            max_search,
            MAX_NEIGHBORS)
            .into_iter()
            .map(|(i, d)| (i, d.into_inner())).collect()
    }


    fn find_entrypoint(layers: &[&[HnswNode]],
                       element: &T,
                       elements: &[T],
                       max_search: usize) -> usize {

        let mut entrypoint = 0;
        for layer in layers {
            let res = Self::search_for_neighbors(
                &layer,
                entrypoint,
                &elements,
                &element,
                max_search,
                1usize);

            entrypoint = res[0].0;
        }

        entrypoint
    }


    fn search_for_neighbors(layer: &[HnswNode],
                            entrypoint: usize,
                            elements: &[T],
                            goal: &T,
                            max_search: usize,
                            max_neighbors: usize)
                            -> Vec<(usize, NotNaN<f32>)> {


        let mut res: MaxSizeHeap<(NotNaN<f32>, usize)> =
            MaxSizeHeap::new(max_search);
        let mut pq: BinaryHeap<RevOrd<_>> = BinaryHeap::new();
        let mut visited = FnvHashSet::default();

        pq.push(RevOrd(
            (elements[entrypoint].dist(&goal), entrypoint)
        ));

        visited.insert(entrypoint);

        while let Some(RevOrd((d, idx))) = pq.pop() {
            if res.is_full() && d > res.peek().unwrap().0 {
                break;
            }

            res.push((d, idx));

            let node = &layer[idx];

            for neighbor_idx in node.neighbors.iter().map(|&n| n as usize) {
                if visited.insert(neighbor_idx) {
                    let distance = elements[neighbor_idx].dist(&goal);

                    if !res.is_full() || distance < res.peek().unwrap().0 {
                        pq.push(RevOrd((distance, neighbor_idx)));
                    }
                }
            }
        }

        res.heap
            .into_sorted_vec()
            .into_iter()
            .take(max_neighbors)
            .map(|(d, idx)| (idx, d))
            .collect()
    }
}


struct MaxSizeHeap<T> {
    heap: BinaryHeap<T>,
    max_size: usize
}

impl<T: Ord> MaxSizeHeap<T> {

    pub fn new(max_size: usize) -> Self {
        MaxSizeHeap {
            heap: BinaryHeap::with_capacity(max_size),
            max_size: max_size
        }
    }

    pub fn push(self: &mut Self, element: T) {
        if !self.is_full() {
            self.heap.push(element);

        } else if element < *self.heap.peek().unwrap() {
            if self.heap.len() >= self.max_size {
                self.heap.pop();
            }

            self.heap.push(element);
        }
    }

    pub fn is_full(self: &Self) -> bool {
        self.heap.len() >= self.max_size
    }

    pub fn peek(self: &Self) -> Option<&T> {
        self.heap.peek()
    }
}


mod tests {
    use super::*;
    use std::mem;
    use types::example::*;

    #[test]
    fn test_hnsw_node_size()
    {
        assert!((MAX_NEIGHBORS) * mem::size_of::<NeighborType>() <= mem::size_of::<HnswNode>());
    }

    #[test]
    fn write_and_load()
    {
        let elements: Vec<FloatElement> =
            (0..100).map(|_| random_float_element()).collect();

        let config = Config {
            num_levels: 4,
            level_multiplier: 6,
            max_search: 100,
            show_progress: false,
        };

        let mut builder = HnswBuilder::new(config, &elements[..]);
        builder.build_index();

        let mut data = Vec::new();
        builder.write(&mut data);

        let index = Hnsw::load(&data[..], &elements[..]);

        assert_eq!(builder.levels.len(), index.levels.len());

        for level in 0..builder.levels.len() {
            assert_eq!(builder.levels[level].len(), index.levels[level].len());

            for i in 0..builder.levels[level].len() {
                assert_eq!(builder.levels[level][i].neighbors,
                           index.levels[level][i].neighbors);
            }
        }
    }

    #[test]
    fn append_elements() {
        let elements: Vec<FloatElement> =
            (0..200).map(|_| random_float_element()).collect();

        let config = Config {
            num_levels: 4,
            level_multiplier: 6,
            max_search: 100,
            show_progress: false,
        };

        // insert half of the elements
        let mut builder = HnswBuilder::new(config, &elements[..100]);
        builder.build_index();

        assert_eq!(4, builder.levels.len());
        assert_eq!(100, builder.levels[3].len());

        let max_search = 200;

        // assert that one arbitrary element is findable (might fail)
        {
            let index = builder.get_index();

            assert!(index.search(&elements[50], max_search)
                    .iter()
                    .any(|&(idx, _)| 50 == idx));
        }

        // insert rest of the elements
        builder.append_elements(&elements[..]);

        assert_eq!(4, builder.levels.len());
        assert_eq!(200, builder.levels[3].len());

        // assert that the same arbitrary element and a newly added one
        // is findable (might fail)
        {
            let index = builder.get_index();

            assert!(index.search(&elements[50], max_search)
                    .iter()
                    .any(|&(idx, _)| 50 == idx));

            assert!(index.search(&elements[150], max_search)
                    .iter()
                    .any(|&(idx, _)| 150 == idx));
        }

    }
}
