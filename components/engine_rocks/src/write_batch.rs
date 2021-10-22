// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use crate::engine::RocksEngine;
use crate::options::RocksWriteOptions;
use crate::util::get_cf_handle;
use engine_traits::{self, Error, Mutable, Result, WriteBatch, WriteBatchExt, WriteOptions};
use rocksdb::{Writable, WriteBatch as RawWriteBatch, DB};

const WRITE_BATCH_MAX_BATCH: usize = 16;
const WRITE_BATCH_LIMIT: usize = 16;

impl WriteBatchExt for RocksEngine {
    type WriteBatch = RocksWriteBatch;

    const WRITE_BATCH_MAX_KEYS: usize = 256;

    fn write_batch(&self) -> RocksWriteBatch {
        let wb = RawWriteBatch::new();
        let options = self.as_inner().get_db_options();
        let enable_batch_vec = options.is_enable_multi_batch_write();
        RocksWriteBatch {
            db: self.as_inner().clone(),
            wbs: vec![wb],
            save_points: vec![],
            index: 0,
            cur_batch_size: 0,
            batch_size_limit: WRITE_BATCH_LIMIT,
            enable_batch_vec,
        }
    }

    fn write_batch_with_cap(&self, cap: usize) -> RocksWriteBatch {
        RocksWriteBatch::with_capacity(self, cap)
    }
}

/// `RocksWriteBatchVec` is for method `multi_batch_write` of RocksDB, which splits a large WriteBatch
/// into many smaller ones and then any thread could help to deal with these small WriteBatch when it
/// is calling `AwaitState` and wait to become leader of WriteGroup. `multi_batch_write` will perform
/// much better than traditional `pipelined_write` when TiKV writes very large data into RocksDB. We
/// will remove this feature when `unordered_write` of RocksDB becomes more stable and becomes compatible
/// with Titan.
pub struct RocksWriteBatch {
    db: Arc<DB>,
    wbs: Vec<RawWriteBatch>,
    save_points: Vec<usize>,
    index: usize,
    cur_batch_size: usize,
    batch_size_limit: usize,
    enable_batch_vec: bool,
}

impl RocksWriteBatch {
    pub fn new(db: Arc<DB>) -> RocksWriteBatch {
        let wb = RawWriteBatch::new();
        let opt = db.get_db_options();
        RocksWriteBatch {
            db,
            wbs: vec![wb],
            save_points: vec![],
            index: 0,
            cur_batch_size: 0,
            batch_size_limit: WRITE_BATCH_LIMIT,
            enable_batch_vec: opt.is_enable_multi_batch_write(),
        }
    }

    pub fn as_inner(&self) -> &[RawWriteBatch] {
        &self.wbs[0..=self.index]
    }

    pub fn as_raw(&self) -> &RawWriteBatch {
        &self.wbs[0]
    }

    pub fn get_db(&self) -> &DB {
        self.db.as_ref()
    }

    /// `check_switch_batch` will split a large WriteBatch into many smaller ones. This is to avoid
    /// a large WriteBatch blocking write_thread too long.
    fn check_switch_batch(&mut self) {
        if self.enable_batch_vec
            && self.batch_size_limit > 0
            && self.cur_batch_size >= self.batch_size_limit
        {
            self.index += 1;
            self.cur_batch_size = 0;
            if self.index >= self.wbs.len() {
                self.wbs.push(RawWriteBatch::default());
            }
        }
        self.cur_batch_size += 1;
    }
}

impl engine_traits::WriteBatch<RocksEngine> for RocksWriteBatch {
    fn with_capacity(engine: &RocksEngine, cap: usize) -> RocksWriteBatch {
        let wb = RawWriteBatch::with_capacity(cap);
        RocksWriteBatch {
            db: engine.as_inner().clone(),
            wbs: vec![wb],
            save_points: vec![],
            index: 0,
            cur_batch_size: 0,
            batch_size_limit: WRITE_BATCH_LIMIT,
            enable_batch_vec: engine.is_support_multi_batch_write(),
        }
    }

    fn write_opt(&self, opts: &WriteOptions) -> Result<()> {
        let opt: RocksWriteOptions = opts.into();
        if self.index > 0 {
            self.get_db()
                .multi_batch_write(self.as_inner(), &opt.into_raw())
                .map_err(Error::Engine)
        } else {
            self.get_db()
                .write_opt(&self.wbs[0], &opt.into_raw())
                .map_err(Error::Engine)
        }
    }

    fn data_size(&self) -> usize {
        self.wbs.iter().fold(0, |a, b| a + b.data_size())
    }

    fn count(&self) -> usize {
        self.cur_batch_size + self.index * self.batch_size_limit
    }

    fn is_empty(&self) -> bool {
        self.wbs[0].is_empty()
    }

    fn should_write_to_engine(&self) -> bool {
        self.index >= WRITE_BATCH_MAX_BATCH
    }

    fn clear(&mut self) {
        for i in 0..=self.index {
            self.wbs[i].clear();
        }
        self.save_points.clear();
        self.index = 0;
        self.cur_batch_size = 0;
    }

    fn set_save_point(&mut self) {
        self.wbs[self.index].set_save_point();
        self.save_points.push(self.index);
    }

    fn pop_save_point(&mut self) -> Result<()> {
        if let Some(x) = self.save_points.pop() {
            return self.wbs[x].pop_save_point().map_err(Error::Engine);
        }
        Err(Error::Engine("no save point".into()))
    }

    fn rollback_to_save_point(&mut self) -> Result<()> {
        if let Some(x) = self.save_points.pop() {
            for i in x + 1..=self.index {
                self.wbs[i].clear();
            }
            self.index = x;
            return self.wbs[x].rollback_to_save_point().map_err(Error::Engine);
        }
        Err(Error::Engine("no save point".into()))
    }

    fn merge(&mut self, other: Self) {
        for i in 0..=other.index {
            self.wbs[self.index].append(other.wbs[other.index].data());
        }
    }
}

impl Mutable for RocksWriteBatch {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index].put(key, value).map_err(Error::Engine)
    }

    fn put_cf(&mut self, cf: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .put_cf(handle, key, value)
            .map_err(Error::Engine)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index].delete(key).map_err(Error::Engine)
    }

    fn delete_cf(&mut self, cf: &str, key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .delete_cf(handle, key)
            .map_err(Error::Engine)
    }

    fn delete_range(&mut self, begin_key: &[u8], end_key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index]
            .delete_range(begin_key, end_key)
            .map_err(Error::Engine)
    }

    fn delete_range_cf(&mut self, cf: &str, begin_key: &[u8], end_key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .delete_range_cf(handle, begin_key, end_key)
            .map_err(Error::Engine)
    }
}

#[cfg(test)]
mod tests {
    use super::super::util::new_engine_opt;
    use super::super::RocksDBOptions;
    use super::*;
    use engine_traits::WriteBatch;
    use rocksdb::DBOptions as RawDBOptions;
    use tempfile::Builder;

    #[test]
    fn test_should_write_to_engine() {
        let path = Builder::new()
            .prefix("test-should-write-to-engine")
            .tempdir()
            .unwrap();
        let opt = RawDBOptions::default();
        opt.enable_multi_batch_write(true);
        opt.enable_unordered_write(false);
        opt.enable_pipelined_write(true);
        let engine = new_engine_opt(
            path.path().join("db").to_str().unwrap(),
            RocksDBOptions::from_raw(opt),
            vec![],
        )
        .unwrap();
        let mut wb = engine.write_batch();
        for _i in 0..RocksEngine::WRITE_BATCH_MAX_KEYS {
            wb.put(b"aaa", b"bbb").unwrap();
        }
        assert!(!wb.should_write_to_engine());
        wb.put(b"aaa", b"bbb").unwrap();
        assert!(wb.should_write_to_engine());
        let mut wb = RocksWriteBatch::with_capacity(&engine, 1024);
        for _i in 0..WRITE_BATCH_MAX_BATCH * WRITE_BATCH_LIMIT {
            wb.put(b"aaa", b"bbb").unwrap();
        }
        assert!(!wb.should_write_to_engine());
        wb.put(b"aaa", b"bbb").unwrap();
        assert!(wb.should_write_to_engine());
        wb.clear();
        assert!(!wb.should_write_to_engine());
    }
}
