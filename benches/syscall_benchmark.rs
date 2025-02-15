use tempfile::{NamedTempFile, TempDir};

use memmap2::MmapMut;
use rand::prelude::SliceRandom;
use rand::Rng;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

const ITERATIONS: usize = 3;
const KEY_SIZE: usize = 24;
const VALUE_SIZE: usize = 2000;
const ELEMENTS: usize = 100_000;

fn human_readable_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}KiB", bytes / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{}MiB", bytes / 1024 / 1024)
    } else if bytes < 1024 * 1024 * 1024 * 1024 {
        format!("{}GiB", bytes / 1024 / 1024 / 1024)
    } else {
        format!("{}TiB", bytes / 1024 / 1024 / 1024 / 1024)
    }
}

fn print_load_time(name: &'static str, duration: Duration) {
    let throughput = ELEMENTS * (KEY_SIZE + VALUE_SIZE) * 1000 / duration.as_millis() as usize;
    println!(
        "{}: Loaded {} items ({}) in {}ms ({}/s)",
        name,
        ELEMENTS,
        human_readable_bytes(ELEMENTS * (KEY_SIZE + VALUE_SIZE)),
        duration.as_millis(),
        human_readable_bytes(throughput),
    );
}

/// Returns pairs of key, value
fn gen_data(count: usize, key_size: usize, value_size: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = vec![];

    for _ in 0..count {
        let key: Vec<u8> = (0..key_size).map(|_| rand::thread_rng().gen()).collect();
        let value: Vec<u8> = (0..value_size).map(|_| rand::thread_rng().gen()).collect();
        pairs.push((key, value));
    }

    pairs
}

fn lmdb_zero_bench(path: &str) {
    let env = unsafe {
        lmdb_zero::EnvBuilder::new()
            .unwrap()
            .open(path, lmdb_zero::open::Flags::empty(), 0o600)
            .unwrap()
    };
    unsafe {
        env.set_mapsize(4096 * 1024 * 1024).unwrap();
    }

    let pairs = gen_data(1000, KEY_SIZE, VALUE_SIZE);

    let db =
        lmdb_zero::Database::open(&env, None, &lmdb_zero::DatabaseOptions::defaults()).unwrap();
    {
        let start = SystemTime::now();
        let txn = lmdb_zero::WriteTransaction::new(&env).unwrap();
        {
            let mut access = txn.access();
            for i in 0..ELEMENTS {
                let (key, value) = &pairs[i % pairs.len()];
                let mut mut_key = key.clone();
                mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                access
                    .put(&db, &mut_key, value, lmdb_zero::put::Flags::empty())
                    .unwrap();
            }
        }
        txn.commit().unwrap();

        let end = SystemTime::now();
        let duration = end.duration_since(start).unwrap();
        print_load_time("lmdb-zero", duration);

        let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
        key_order.shuffle(&mut rand::thread_rng());

        let txn = lmdb_zero::ReadTransaction::new(&env).unwrap();
        {
            for _ in 0..ITERATIONS {
                let start = SystemTime::now();
                let access = txn.access();
                let mut checksum = 0u64;
                let mut expected_checksum = 0u64;
                for &i in &key_order {
                    let (key, value) = &pairs[i % pairs.len()];
                    let mut mut_key = key.clone();
                    mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                    let result: &[u8] = access.get(&db, &mut_key).unwrap();
                    checksum += result[0] as u64;
                    expected_checksum += value[0] as u64;
                }
                assert_eq!(checksum, expected_checksum);
                let end = SystemTime::now();
                let duration = end.duration_since(start).unwrap();
                println!(
                    "lmdb-zero: Random read {} items in {}ms",
                    ELEMENTS,
                    duration.as_millis()
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn uring_bench(path: &Path) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .unwrap();

    let pairs = gen_data(1000, KEY_SIZE, VALUE_SIZE);

    let start = SystemTime::now();
    {
        for i in 0..ELEMENTS {
            let (key, value) = &pairs[i % pairs.len()];
            let mut mut_key = key.clone();
            mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            file.write_all(&mut_key).unwrap();
            file.write_all(value).unwrap();
        }
    }
    file.sync_all().unwrap();

    let end = SystemTime::now();
    let duration = end.duration_since(start).unwrap();
    print_load_time("uring_read()/write()", duration);

    let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
    key_order.shuffle(&mut rand::thread_rng());

    {
        for _ in 0..ITERATIONS {
            let start = SystemTime::now();

            let uring_entries = 10usize;
            let mut ring = io_uring::IoUring::new(uring_entries as u32).unwrap();
            let mut buffers = vec![vec![0u8; VALUE_SIZE]; uring_entries];

            let mut checksum = 0u64;
            let mut expected_checksum = 0u64;
            for (uring_counter, &i) in key_order.iter().enumerate() {
                let (key, value) = &pairs[i % pairs.len()];
                let mut mut_key = key.clone();
                mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                let offset = i * (mut_key.len() + value.len()) + mut_key.len();

                expected_checksum += value[0] as u64;

                let buffer_index = uring_counter % uring_entries;
                let buf = &mut buffers[buffer_index];
                let iovec = libc::iovec {
                    iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                    iov_len: buf.len(),
                };
                let read_e =
                    io_uring::opcode::Readv::new(io_uring::types::Fd(file.as_raw_fd()), &iovec, 1)
                        .offset(offset as i64)
                        .build()
                        .user_data(buffer_index as u64);

                unsafe {
                    ring.submission().push(&read_e).unwrap();
                }
                ring.submit().unwrap();

                if uring_counter % uring_entries == (uring_entries - 1) {
                    ring.submit_and_wait(uring_entries).unwrap();
                    for _ in 0..uring_entries {
                        let cqe = ring.completion().next().unwrap();
                        checksum += buffers[cqe.user_data() as usize][0] as u64;
                    }
                }
            }
            assert_eq!(checksum, expected_checksum);
            let end = SystemTime::now();
            let duration = end.duration_since(start).unwrap();
            println!(
                "uring_read()/write(): Random read {} items in {}ms",
                ELEMENTS,
                duration.as_millis()
            );
        }
    }
}

fn readwrite_bench(path: &Path) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .unwrap();

    let pairs = gen_data(1000, KEY_SIZE, VALUE_SIZE);

    let start = SystemTime::now();
    {
        for i in 0..ELEMENTS {
            let (key, value) = &pairs[i % pairs.len()];
            let mut mut_key = key.clone();
            mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            file.write_all(&mut_key).unwrap();
            file.write_all(value).unwrap();
        }
    }
    file.sync_all().unwrap();

    let end = SystemTime::now();
    let duration = end.duration_since(start).unwrap();
    print_load_time("read()/write()", duration);

    let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
    key_order.shuffle(&mut rand::thread_rng());

    {
        for _ in 0..ITERATIONS {
            let start = SystemTime::now();
            let mut checksum = 0u64;
            let mut expected_checksum = 0u64;
            let mut buffer = vec![0u8; 2000];
            for &i in &key_order {
                let (key, value) = &pairs[i % pairs.len()];
                let mut mut_key = key.clone();
                mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                let offset = i * (mut_key.len() + value.len()) + mut_key.len();

                file.seek(SeekFrom::Start(offset as u64)).unwrap();
                file.read_exact(&mut buffer).unwrap();
                checksum += buffer[0] as u64;
                expected_checksum += value[0] as u64;
            }
            assert_eq!(checksum, expected_checksum);
            let end = SystemTime::now();
            let duration = end.duration_since(start).unwrap();
            println!(
                "read()/write(): Random read {} items in {}ms",
                ELEMENTS,
                duration.as_millis()
            );
        }
    }
}

fn mmap_bench(path: &Path) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .unwrap();

    file.set_len(4 * 1024 * 1024 * 1024).unwrap();

    let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };
    drop(file);

    let pairs = gen_data(1000, KEY_SIZE, VALUE_SIZE);

    let mut write_index = 0;
    let start = SystemTime::now();
    {
        for i in 0..ELEMENTS {
            let (key, value) = &pairs[i % pairs.len()];
            let mut mut_key = key.clone();
            mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            mmap[write_index..(write_index + mut_key.len())].copy_from_slice(&mut_key);
            write_index += mut_key.len();
            mmap[write_index..(write_index + value.len())].copy_from_slice(value);
            write_index += value.len();
        }
    }
    mmap.flush().unwrap();

    let end = SystemTime::now();
    let duration = end.duration_since(start).unwrap();
    print_load_time("mmap()", duration);

    let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
    key_order.shuffle(&mut rand::thread_rng());

    {
        for _ in 0..ITERATIONS {
            let start = SystemTime::now();
            let mut checksum = 0u64;
            let mut expected_checksum = 0u64;
            for &i in &key_order {
                let (key, value) = &pairs[i % pairs.len()];
                let mut mut_key = key.clone();
                mut_key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                let offset = i * (mut_key.len() + value.len()) + mut_key.len();
                let buffer = &mmap[offset..(offset + value.len())];
                checksum += buffer[0] as u64;
                expected_checksum += value[0] as u64;
            }
            assert_eq!(checksum, expected_checksum);
            let end = SystemTime::now();
            let duration = end.duration_since(start).unwrap();
            println!(
                "mmap(): Random read {} items in {}ms",
                ELEMENTS,
                duration.as_millis()
            );
        }
    }
}

fn main() {
    // Benchmark lmdb against raw read()/write() performance
    {
        let tmpfile: TempDir = tempfile::tempdir().unwrap();
        let path = tmpfile.path().to_str().unwrap();
        lmdb_zero_bench(path);
    }
    {
        let tmpfile: NamedTempFile = NamedTempFile::new().unwrap();
        readwrite_bench(tmpfile.path());
    }
    #[cfg(target_os = "linux")]
    {
        let tmpfile: NamedTempFile = NamedTempFile::new().unwrap();
        uring_bench(tmpfile.path());
    }
    {
        let tmpfile: NamedTempFile = NamedTempFile::new().unwrap();
        mmap_bench(tmpfile.path());
    }
}
