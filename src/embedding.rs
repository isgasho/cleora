use crate::persistence::embedding::EmbeddingPersistor;
use crate::persistence::entity::EntityMappingPersistor;
use crate::persistence::sparse_matrix::SparseMatrixPersistor;
use crate::sparse_matrix::SparseMatrix;
use fnv::FnvHasher;
use log::info;
use memmap::MmapMut;
use rayon::prelude::*;
use std::fs;
use std::fs::OpenOptions;
use std::hash::Hasher;
use std::sync::Arc;

/// Calculate embeddings in memory.
pub fn calculate_embeddings<T1, T2, T3>(
    sparse_matrix: &mut SparseMatrix<T1>,
    max_iter: u8,
    entity_mapping_persistor: Arc<T2>,
    embedding_persistor: &mut T3,
) where
    T1: SparseMatrixPersistor + Sync,
    T2: EntityMappingPersistor + Sync,
    T3: EmbeddingPersistor,
{
    sparse_matrix.normalize();

    let mult = MatrixMultiplicator {
        dimension: sparse_matrix.dimension,
        sparse_matrix_id: sparse_matrix.get_id(),
        sparse_matrix_persistor: &sparse_matrix.sparse_matrix_persistor,
    };
    let init = mult.initialize();
    let res = mult.propagate(max_iter, init);
    mult.persist(res, entity_mapping_persistor, embedding_persistor);

    info!("Finalizing embeddings calculations!")
}

/// Provides matrix multiplication based on sparse matrix data.
#[derive(Debug)]
pub struct MatrixMultiplicator<'a, T: SparseMatrixPersistor + Sync> {
    pub dimension: u16,
    pub sparse_matrix_id: String,
    pub sparse_matrix_persistor: &'a T,
}

impl<'a, T> MatrixMultiplicator<'a, T>
where
    T: SparseMatrixPersistor + Sync,
{
    fn initialize(&self) -> Vec<Vec<f32>> {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter();

        info!(
            "Start initialization. Dims: {}, entities: {}.",
            self.dimension, entities_count
        );

        // no specific requirement (ca be lower as well)
        let max_hash = 8 * 1024 * 1024;
        let max_hash_float = max_hash as f32;

        let result: Vec<Vec<f32>> = (0..self.dimension)
            .into_par_iter()
            .map(|i| {
                let mut col: Vec<f32> = Vec::with_capacity(entities_count as usize);
                for j in 0..entities_count {
                    let hsh = self.sparse_matrix_persistor.get_hash(j);
                    if hsh != -1 {
                        let col_value =
                            ((hash(hsh + (i as i64)) % max_hash) as f32) / max_hash_float;
                        col.insert(j as usize, col_value);
                    }
                }
                col
            })
            .collect();

        info!(
            "Done initializing. Dims: {}, entities: {}.",
            self.dimension, entities_count
        );
        result
    }

    fn propagate(&self, max_iter: u8, res: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        info!("Start propagating. Number of iterations: {}.", max_iter);

        let entities_count = self.sparse_matrix_persistor.get_entity_counter();
        let mut new_res = res;
        for i in 0..max_iter {
            let next = self.next_power(new_res);
            new_res = self.normalize(next);
            info!(
                "Done iter: {}. Dims: {}, entities: {}, num data points: {}.",
                i,
                self.dimension,
                entities_count,
                self.sparse_matrix_persistor.get_amount_of_data()
            );
        }
        info!("Done propagating.");
        new_res
    }

    fn next_power(&self, res: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter() as usize;
        let rnew = Self::zero_2d(entities_count, self.dimension as usize);

        let amount_of_data = self.sparse_matrix_persistor.get_amount_of_data();

        let result: Vec<Vec<f32>> = res
            .into_par_iter()
            .zip(rnew)
            .update(|data| {
                let (res_col, rnew_col) = data;
                for j in 0..amount_of_data {
                    let entry = self.sparse_matrix_persistor.get_entry(j);
                    let elem = rnew_col.get_mut(entry.row as usize).unwrap();
                    let value = res_col.get(entry.col as usize).unwrap();
                    *elem += *value * entry.value
                }
            })
            .map(|data| data.1)
            .collect();

        result
    }

    fn zero_2d(row: usize, col: usize) -> Vec<Vec<f32>> {
        let mut res: Vec<Vec<f32>> = Vec::with_capacity(col);
        for i in 0..col {
            let col = vec![0f32; row];
            res.insert(i, col);
        }
        res
    }

    fn normalize(&self, res: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter() as usize;
        let mut row_sum = vec![0f32; entities_count];

        for i in 0..(self.dimension as usize) {
            for j in 0..entities_count {
                let sum = row_sum.get_mut(j).unwrap();
                let col: &Vec<f32> = res.get(i).unwrap();
                let value = col.get(j).unwrap();
                *sum += value.powi(2)
            }
        }

        let row_sum = Arc::new(row_sum);
        let result: Vec<Vec<f32>> = res
            .into_par_iter()
            .update(|col| {
                for j in 0..entities_count {
                    let value = col.get_mut(j).unwrap();
                    let sum = row_sum.get(j).unwrap();
                    *value /= sum.sqrt();
                }
            })
            .collect();

        result
    }

    fn persist<T1, T2>(
        &self,
        res: Vec<Vec<f32>>,
        entity_mapping_persistor: Arc<T1>,
        embedding_persistor: &mut T2,
    ) where
        T1: EntityMappingPersistor,
        T2: EmbeddingPersistor,
    {
        info!("Start saving embeddings.");

        let entities_count = self.sparse_matrix_persistor.get_entity_counter();
        embedding_persistor.put_metadata(entities_count, self.dimension);

        for i in 0..entities_count {
            let hash = self.sparse_matrix_persistor.get_hash(i);
            let entity_name_opt = entity_mapping_persistor.get_entity(hash as u64);
            if let Some(entity_name) = entity_name_opt {
                let hash_occur = self
                    .sparse_matrix_persistor
                    .get_hash_occurrence(hash as u64);
                let mut embedding: Vec<f32> = Vec::with_capacity(self.dimension as usize);
                for j in 0..(self.dimension as usize) {
                    let col: &Vec<f32> = res.get(j).unwrap();
                    let value = col.get(i as usize).unwrap();
                    embedding.insert(j, *value);
                }
                embedding_persistor.put_data(entity_name, hash_occur, embedding);
            };
        }

        embedding_persistor.finish();

        info!("Done saving embeddings.");
    }
}

fn hash(num: i64) -> i64 {
    let mut hasher = FnvHasher::default();
    hasher.write_i64(num);
    hasher.finish() as i64
}

/// Calculate embeddings with memory-mapped files.
pub fn calculate_embeddings_mmap<T1, T2, T3>(
    sparse_matrix: &mut SparseMatrix<T1>,
    max_iter: u8,
    entity_mapping_persistor: Arc<T2>,
    embedding_persistor: &mut T3,
) where
    T1: SparseMatrixPersistor + Sync,
    T2: EntityMappingPersistor + Sync,
    T3: EmbeddingPersistor,
{
    sparse_matrix.normalize();

    let mult = MatrixMultiplicatorMMap {
        dimension: sparse_matrix.dimension,
        sparse_matrix_id: sparse_matrix.get_id(),
        sparse_matrix_persistor: &sparse_matrix.sparse_matrix_persistor,
    };
    let init = mult.initialize();
    let res = mult.propagate(max_iter, init);
    mult.persist(res, entity_mapping_persistor, embedding_persistor);

    fs::remove_file(format!("{}_matrix_{}", sparse_matrix.get_id(), max_iter)).unwrap();

    info!("Finalizing embeddings calculations!")
}

/// Provides matrix multiplication based on sparse matrix data.
#[derive(Debug)]
pub struct MatrixMultiplicatorMMap<'a, T: SparseMatrixPersistor + Sync> {
    pub dimension: u16,
    pub sparse_matrix_id: String,
    pub sparse_matrix_persistor: &'a T,
}

impl<'a, T> MatrixMultiplicatorMMap<'a, T>
where
    T: SparseMatrixPersistor + Sync,
{
    fn initialize(&self) -> MmapMut {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter();

        info!(
            "Start initialization. Dims: {}, entities: {}.",
            self.dimension, entities_count
        );

        let number_of_bytes = entities_count as u64 * self.dimension as u64 * 4;
        let file_name = format!("{}_matrix_0", self.sparse_matrix_id);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(file_name)
            .unwrap();
        file.set_len(number_of_bytes).unwrap();
        let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };

        // no specific requirement (ca be lower as well)
        let max_hash = 8 * 1024 * 1024;
        let max_hash_float = max_hash as f32;

        mmap.par_chunks_mut((entities_count * 4) as usize)
            .enumerate()
            .for_each(|(i, chunk)| {
                // i - number of dimension
                // chunk - column/vector of bytes
                for j in 0..entities_count as usize {
                    let hsh = self.sparse_matrix_persistor.get_hash(j as u32);
                    if hsh != -1 {
                        let col_value =
                            ((hash(hsh + (i as i64)) % max_hash) as f32) / max_hash_float;

                        let start_idx = j * 4;
                        let end_idx = start_idx + 4;
                        let pointer: *mut u8 = (&mut chunk[start_idx..end_idx]).as_mut_ptr();
                        unsafe {
                            let value = pointer as *mut f32;
                            *value = col_value;
                        };
                    }
                }
            });

        info!(
            "Done initializing. Dims: {}, entities: {}.",
            self.dimension, entities_count
        );

        mmap.flush();
        mmap
    }

    fn propagate(&self, max_iter: u8, res: MmapMut) -> MmapMut {
        info!("Start propagating. Number of iterations: {}.", max_iter);

        let entities_count = self.sparse_matrix_persistor.get_entity_counter();
        let mut new_res = res;
        for i in 0..max_iter {
            let next = self.next_power(i, new_res);
            new_res = self.normalize(next);
            fs::remove_file(format!("{}_matrix_{}", self.sparse_matrix_id, i)).unwrap();
            info!(
                "Done iter: {}. Dims: {}, entities: {}, num data points: {}.",
                i,
                self.dimension,
                entities_count,
                self.sparse_matrix_persistor.get_amount_of_data()
            );
        }
        info!("Done propagating.");
        new_res
    }

    fn next_power(&self, iteration: u8, res: MmapMut) -> MmapMut {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter() as usize;

        let number_of_bytes = entities_count as u64 * self.dimension as u64 * 4;
        let file_name = format!("{}_matrix_{}", self.sparse_matrix_id, iteration + 1);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(file_name)
            .unwrap();
        file.set_len(number_of_bytes).unwrap();
        let mut mmap_output = unsafe { MmapMut::map_mut(&file).unwrap() };

        let amount_of_data = self.sparse_matrix_persistor.get_amount_of_data();

        let input = Arc::new(res);
        mmap_output
            .par_chunks_mut(entities_count * 4)
            .enumerate()
            .for_each_with(input, |input, (i, chunk)| {
                for j in 0..amount_of_data {
                    let entry = self.sparse_matrix_persistor.get_entry(j);

                    let start_idx_input = ((i * entities_count) + entry.col as usize) * 4;
                    let end_idx_input = start_idx_input + 4;
                    let pointer: *const u8 = (&input[start_idx_input..end_idx_input]).as_ptr();
                    let input_value = unsafe {
                        let value = pointer as *const f32;
                        *value
                    };

                    let start_idx_output = entry.row as usize * 4;
                    let end_idx_output = start_idx_output + 4;
                    let pointer: *mut u8 =
                        (&mut chunk[start_idx_output..end_idx_output]).as_mut_ptr();
                    unsafe {
                        let value = pointer as *mut f32;
                        *value += input_value * entry.value;
                    };
                }
            });

        mmap_output.flush();
        mmap_output
    }

    fn normalize(&self, mut res: MmapMut) -> MmapMut {
        let entities_count = self.sparse_matrix_persistor.get_entity_counter() as usize;
        let mut row_sum = vec![0f32; entities_count];

        for i in 0..(self.dimension as usize) {
            for j in 0..entities_count {
                let sum = row_sum.get_mut(j).unwrap();

                let start_idx = ((i * entities_count) + j) * 4;
                let end_idx = start_idx + 4;
                let pointer: *const u8 = (&res[start_idx..end_idx]).as_ptr();
                let value = unsafe {
                    let value = pointer as *const f32;
                    *value
                };

                *sum += value.powi(2)
            }
        }

        let row_sum = Arc::new(row_sum);
        res.par_chunks_mut(entities_count * 4)
            .enumerate()
            .for_each(|(_i, chunk)| {
                // i - number of dimension
                // chunk - column/vector of bytes
                for j in 0..entities_count {
                    let sum = *row_sum.get(j).unwrap();

                    let start_idx = j * 4;
                    let end_idx = start_idx + 4;
                    let pointer: *mut u8 = (&mut chunk[start_idx..end_idx]).as_mut_ptr();
                    unsafe {
                        let value = pointer as *mut f32;
                        *value /= sum.sqrt();
                    };
                }
            });

        res.flush();
        res
    }

    fn persist<T1, T2>(
        &self,
        res: MmapMut,
        entity_mapping_persistor: Arc<T1>,
        embedding_persistor: &mut T2,
    ) where
        T1: EntityMappingPersistor,
        T2: EmbeddingPersistor,
    {
        info!("Start saving embeddings.");

        let entities_count = self.sparse_matrix_persistor.get_entity_counter();
        embedding_persistor.put_metadata(entities_count, self.dimension);

        for i in 0..entities_count {
            let hash = self.sparse_matrix_persistor.get_hash(i);
            let entity_name_opt = entity_mapping_persistor.get_entity(hash as u64);
            if let Some(entity_name) = entity_name_opt {
                let hash_occur = self
                    .sparse_matrix_persistor
                    .get_hash_occurrence(hash as u64);
                let mut embedding: Vec<f32> = Vec::with_capacity(self.dimension as usize);
                for j in 0..(self.dimension as usize) {
                    let start_idx = ((j * entities_count as usize) + i as usize) * 4;
                    let end_idx = start_idx + 4;
                    let pointer: *const u8 = (&res[start_idx..end_idx]).as_ptr();
                    let value = unsafe {
                        let value = pointer as *const f32;
                        *value
                    };

                    embedding.insert(j, value);
                }
                embedding_persistor.put_data(entity_name, hash_occur, embedding);
            };
        }

        embedding_persistor.finish();

        info!("Done saving embeddings.");
    }
}
