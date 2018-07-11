/// The `page_table` module implements a fast way to process requests to smart contracts including
/// simple transactions.  Benchmarks show about 125,791 ns/iter of 256 Tx's which is near 2m TPS in
/// single threaded mode.
///
/// How `Call`s travely through the pipeline can be seen in the `bench::move_funds` and test
/// 1. pt.acquire_memory_lock(&transactions, &mut lock);
/// - Memory is locked.  Any pages referenced in the Tx's are locked while the batch of
/// `transcations` is moving throuhg the pipeline.  
/// 2. pt.validate_debits(&transactions, &lock, &mut from_pages);
/// - Memory is checked for funds. All pages have a `balance`.  A caller spends this balance, so
/// all the caller pages are checked for funds.
/// 3. pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
/// - New pages might need to be allocated.  This first finds if any pages need to be allocated.
/// 4. pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
/// - PageTable WRITE lock.  This operation requires us to lock the page table and allocaet
/// pages.
/// 5. pt.load_and_execute(&transactions, &mut from_pages, &mut to_pages);
/// - Spends are actually moved.
/// 6. pt.release_memory_lock(&transactions, &lock);
/// - Memory is released
///
/// This can be safely pipelined with an `unsafe`.  The memory lock ensures that all pages
/// traveling through the system are non overlapping, and using the WRITE lock durring allocation
/// ensures that they are present when the READ lock is held.  To safely execute the contracts in
/// parallel the READ lock must be held while loading the pages, and executing the contract.
///
///
/// Main differences from the `Bank`
/// 1. `last_id` and `last_hash` are PoH identifiers, these are processed outside of this pipeline
/// 2. `Call.version` is used to prevent duplicate spends, each PublicKey has 2**64 number of calls
/// it can make.
/// 3. `Page` entry is similar to an `Account`, but it also has a `contract` that owns it.  That
///    tag allows the contract to Write to the memory owned by the page.  Contracts can spend money
use bincode::deserialize;
use rand::{thread_rng, Rng};
use std::collections::{BTreeMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::{Mutex, RwLock};

//these types vs just u64 had a 40% impact on perf without FastHasher
type Hash = [u64; 4];
type PublicKey = [u64; 4];
type Signature = [u64; 8];

const DEFAULT_CONTRACT: [u64;4] = [0u64;4];

/// SYSTEM interface, same for very contract, methods 0 to 127
/// method 0
/// reallocate
/// spend the funds from the call to the first recepient
pub fn SYSTEM_0_realloc(call: &Call,
                        pages: &mut Vec<Page>,
                        user_data: Vec<u8>) {
    let size = deserialize(user_data).unwrap();
    pages[0].memory.resize(size);
}
/// method 1
/// assign
/// assign the page to a contract
pub fn SYSTEM_1_assign(call: &Call,
                       pages: &mut Vec<Page>,
                       user_data: Vec<u8>) {
    let contract = deserialize(user_data).unwrap();
    if call.contract == DEFAULT_CONTRACT && pages[0].contract == DEFAULT_CONTRACT {
        pages[0].contract = contract;
    }
}
/// DEFAULT_CONTRACT interface
/// All contracts start with 128
/// method 128
/// move_funds
/// spend the funds from the call to the first recepient
pub fn DEFAULT_CONTRACT_128_move_funds(call: &Call,
                  pages: &mut Vec<Page>,
                  user_data: Vec<u8>) {
    let amount = deserialize(user_data).unwrap();
    pages[0].balance -= amount;
    pages[1].balance += amount;
} 
 
//157,173 ns/iter vs 125,791 ns/iter with using this hasher, 110,000 ns with u64 as the key
struct FastHasher {
    //generates some seeds to pluck bytes out of the keys
    //since these are random every time we create a new hashset, there is no way to
    //predict what these will be for the node or the network
    rand32: [usize; 8],
    rand8: [usize; 8],
    data64: [u8; 64],
    len: usize,
}

impl Hasher for FastHasher {
    /// pluck the bytes out of the data and glue it into a u64
    fn finish(&self) -> u64 {
        let seed = if self.len == 32 {
            self.rand32
        } else if self.len == 8 {
            self.rand8
        } else {
            assert!(false);
            [0xfefefefefefefefe; 8]
        };
        (self.data64[seed[0]] as u64)
            | ((self.data64[seed[1]] as u64) << (1 * 8))
            | ((self.data64[seed[2]] as u64) << (2 * 8))
            | ((self.data64[seed[3]] as u64) << (3 * 8))
            | ((self.data64[seed[4]] as u64) << (4 * 8))
            | ((self.data64[seed[5]] as u64) << (5 * 8))
            | ((self.data64[seed[6]] as u64) << (6 * 8))
            | ((self.data64[seed[7]] as u64) << (7 * 8))
    }
    fn write(&mut self, bytes: &[u8]) {
        assert!(bytes.len() < 64);
        self.data64[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
    }
}

impl FastHasher {
    fn gen_rand(size: usize, rand: &mut [usize]) {
        let mut seed: Vec<_> = (0..size).collect();
        thread_rng().shuffle(&mut seed);
        rand[0..8].copy_from_slice(&seed[0..8]);
    }

    fn new() -> FastHasher {
        let mut rand32 = [0usize; 8];
        Self::gen_rand(32, &mut rand32);
        let mut rand8 = [0usize; 8];
        Self::gen_rand(8, &mut rand8);

        FastHasher {
            rand32: rand32,
            rand8: rand8,
            data64: [0; 64],
            len: 0,
        }
    }
}

impl BuildHasher for FastHasher {
    type Hasher = FastHasher;
    fn build_hasher(&self) -> Self::Hasher {
        FastHasher {
            rand32: self.rand32.clone(),
            rand8: self.rand8.clone(),
            data64: [0; 64],
            len: 0,
        }
    }
}

/// Generic Page for the PageTable
#[derive(Clone)]
pub struct Page {
    /// key that indexes this page
    /// proove ownership of this key to spend from this Page
    owner: PublicKey,
    /// contract that owns this page
    /// contract can write to the data that is pointed to by `pointer`
    contract: PublicKey,
    /// balance that belongs to owner
    balance: u64,
    /// version of the structure
    version: u64,
    /// hash of the page data
    memhash: Hash,
    /// The following could be in a separate structure
    memory: Vec<u8>,
}

impl Default for Page {
    fn default() -> Page {
        Page {
            owner: [0, 0, 0, 0],
            contract: [0, 0, 0, 0],
            balance: 0,
            version: 0,
            memhash: [0, 0, 0, 0],
            memory: vec![],
        }
    }
}
/// Call definition
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Call {

    /// Signatures and Keys
    /// proofs[0] is the signature
    /// number of proofs of ownership of `inkeys`, `owner` is proven by the signature
    proofs: Vec<Option<Signature>>, 
    /// number of keys to load, aka the to key
    /// inkeys[0] is the caller's key
    inkeys: Vec<PublicKey>,

    /// PoH data
    /// last id PoH observed by the sender
    last_id: u64,
    /// last PoH hash observed by the sender
    last_hash: Hash, 
 
    /// Program
    /// the address of the program we want to call
    contract: PublicKey,
    /// OS scheduling fee
    fee: u64,
    /// struct version to prevent duplicate spends
    /// Calls with a version <= Page.version are rejected
    version: u64,
    /// method to call in the contract
    method: u8,
    /// usedata in bytes
    user_data: Vec<u8>,
}

/// simple transaction over a Call
/// TODO: The pipeline will need to pass the `destination` public keys in a side buffer to generalize
/// this
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
impl struct Call {
    pub fn new_tx(from: PublicKey, last_id: u64, last_hash: Hash, amount: u64, fee: u64, version: u64, to: PublicKey) {
        Call {
            last_id: last_id,
            last_hash: last_hash,
            caller: from,
            inkeys: vec![to],
            proofs: vec![],
            contract: [0; 8],
            amount: amount,
            fee: fee,
            version: version,
            user_data: vec![],
        }
    }
}

pub struct AllocatedPages {
    max: usize,
    allocated: BTreeMap<PublicKey, usize>,
    free: Vec<usize>,
}

impl AllocatedPages {
    pub fn new() -> Self {
        AllocatedPages {
            max: 0,
            allocated: BTreeMap::new(),
            free: vec![],
        }
    }
    pub fn lookup(&self, key: &PublicKey) -> Option<usize> {
        self.allocated.get(key).cloned().map(|x| x as usize)
    }
    pub fn free(&mut self, key: &PublicKey) {
        let page = self.lookup(key).unwrap();
        self.free.push(page);
        self.allocated.remove(key);
    }
    pub fn allocate(&mut self, key: PublicKey) -> usize {
        let page = if let Some(p) = self.free.pop() {
            p
        } else {
            let p = self.max;
            self.max += 1;
            p
        };
        let old = self.allocated.insert(key, page);
        assert!(old.is_none());
        page
    }
}

pub struct PageTable {
    /// entries of Pages
    page_table: Vec<Page>,
    /// a map from page public keys, to index into the page_table
    allocated_pages: RwLock<AllocatedPages>,
    /// locked pages that are currently processed
    mem_locks: Mutex<HashSet<PublicKey, FastHasher>>,
}

impl PageTable {
    pub fn new() -> Self {
        PageTable {
            page_table: vec![],
            allocated_pages: RwLock::new(AllocatedPages::new()),
            mem_locks: Mutex::new(HashSet::with_hasher(FastHasher::new())),
        }
    }
    pub fn acquire_memory_lock(&self, packet: &Vec<Call>, acquired_memory: &mut Vec<bool>) {
        //holds mem_locks mutex
        let mut mem_locks = self.mem_locks.lock().unwrap();
        for (i, p) in packet.iter().enumerate() {
            let collision = mem_locks.contains(&p.call.caller) || mem_locks.contains(&p.destination);
            acquired_memory[i] = !collision;
            if !collision {
                mem_locks.insert(p.call.caller);
                mem_locks.insert(p.destination);
            }
        }
    }
    pub fn validate_debits(
        &mut self,
        packet: &Vec<Call>,
        acquired_memory: &Vec<bool>,
        from_pages: &mut Vec<Option<usize>>,
    ) {
        //holds page table READ lock
        let allocated_pages = self.allocated_pages.read().unwrap();
        for (i, tx) in packet.iter().enumerate() {
            from_pages[i] = None;
            if !acquired_memory[i] {
                continue;
            }
            if let Some(memix) = allocated_pages.lookup(&tx.call.caller) {
                if let Some(page) = self.page_table.get(memix) {
                    assert_eq!(page.owner, tx.call.caller);
                    if page.version >= tx.call.version {
                        continue;
                    }
                    // from pages must belong to the contract
                    if page.contract != tx.call.contract {
                        continue;
                    }
                    if page.balance >= tx.call.fee + tx.call.amount {
                        from_pages[i] = Some(memix);
                    }
                }
            }
        }
    }
    pub fn find_new_keys(
        &mut self,
        packet: &Vec<Call>,
        from_pages: &Vec<Option<usize>>,
        to_pages: &mut Vec<Option<usize>>,
    ) {
        //holds READ lock to the page table
        let allocated_pages = self.allocated_pages.read().unwrap();
        for (i, tx) in packet.iter().enumerate() {
            to_pages[i] = None;
            if from_pages[i].is_none() {
                continue;
            }
            to_pages[i] = allocated_pages.lookup(&tx.destination);
        }
    }
    #[cfg(test)]
    pub fn force_allocate(&mut self, packet: &Vec<Call>, owner: bool, amount: u64) {
        let mut allocated_pages = self.allocated_pages.write().unwrap();
        for tx in packet.iter() {
            let key = if owner {
                tx.call.caller
            } else {
                tx.destination
            };
            let page = Page {
                owner: key,
                contract: tx.call.contract,
                balance: amount,
                version: 0,
                size: 0,
                pointer: 0,
                memhash: [0, 0, 0, 0],
            };
            let ix = allocated_pages.allocate(key) as usize;
            if self.page_table.len() <= ix {
                self.page_table.resize(ix + 1, page);
            } else {
                self.page_table[ix] = page;
            }
        }
    }
    #[cfg(test)]
    pub fn check_pages(
        &self,
        txs: &Vec<Call>,
        from_pages: &Vec<Option<usize>>,
        to_pages: &Vec<Option<usize>>,
    ) {
        //while we hold the write lock, this is where we can create another anonymouns page
        //with copy and write, and send this table to the vote signer
        for (i, tx) in txs.iter().enumerate() {
            if from_pages[i].is_some() {
                assert_eq!(tx.call.caller, self.page_table[from_pages[i].unwrap()].owner);
            }
            if to_pages[i].is_some() {
                assert_eq!(tx.destination, self.page_table[to_pages[i].unwrap()].owner);
            }
        }
    }
    pub fn allocate_keys(
        &mut self,
        packet: &Vec<Call>,
        from_pages: &Vec<Option<usize>>,
        to_pages: &mut Vec<Option<usize>>,
    ) {
        //holds WRITE lock to the page table, since we are adding keys and resizing the table here
        let mut allocated_pages = self.allocated_pages.write().unwrap();
        //while we hold the write lock, this is where we can create another anonymouns page
        //with copy and write, and send this table to the vote signer
        for (i, tx) in packet.iter().enumerate() {
            if from_pages[i].is_none() {
                continue;
            }
            if to_pages[i].is_some() {
                continue;
            }
            let page = Page {
                owner: tx.destination,
                contract: tx.call.contract,
                version: 0,
                balance: 0,
                size: 0,
                pointer: 0,
                memhash: [0, 0, 0, 0],
            };
            let ix = allocated_pages.allocate(tx.destination) as usize;
            if self.page_table.len() <= ix {
                trace!("reallocating page table {}", ix);
                //this may reallocate the page_table, that is one of the reasons why we need to hold the write lock
                self.page_table.resize(ix + 1, Page::default());
            }
            self.page_table[ix] = page;
            to_pages[i] = Some(ix);
        }
    }

    fn load_pages(
        &mut self,
        // Pass the _allocated_pages argument to make sure the lock is held for this call
        _allocated_pages: &AllocatedPages,
        packet: &Vec<Call>,
        from_pages: &Vec<Option<usize>>,
        to_pages: &Vec<Vec<usize>>,
        load_pages: &mut Vec<Option<(Vec<&mut Page>)>>,
    ) {
        for (i,tx) in packet.iter().enumerate() {
            load_pages[i] = None;
            if let (Some(fix), true) = (from_pages[i], to_pages[i].len() == tx.inkeys.len()) {
                let to_mems = to_pages[i].iter().map(|ix| &mut self.page_table[ix] as *mut Page);  
                let mem_refs = unsafe {
                    to_mem_ptrs.map(|x| x as &mut Page).collect()
                }
                load_pages[i] = Some(mem_refs);
            } 
        };
    }

    /// parallel execution of contracts
    fn par_execute(
        // Pass the _allocated_pages argument to make sure the lock is held for this call
        _allocated_pages: &AllocatedPages,
        packet: &Vec<Call>,
        loaded_page_table: &Vec<Option<(Vec<&mut Page>)>>,
    ) {
        packet.iter().zip(&loaded_page_table).into_par_iter().map(|(tx, maybe_pages)| {
            if let Some(loaded_pages) = maybe_pages {
                // Spend the fee first
                loaded_pages[0].balance -= tx.fee;
                let pre_call_total_spendable = loaded_pages.map(|(page,proof)| {
                    if page.contract == tx.contract {
                        page.balance;
                    }
                }).sum();

                let pre_call_total_unspendable = loaded_pages.map(|(page,proof)| {
                    if page.contract != tx.contract {
                        page.balance;
                    }
                }).sum(); 
                let pre_call_total = pre_call_total_spendable + pre_call_total_unspendable;

                // TODO(anatoly): Load actual memory
 
                let call_pages = loaded_pages.cloned().collect();
                // Find the method
                match (tx.contract, tx.method) {
                    // system interface
                    // everyone has the same reallocate
                    (_,0) => DEFAULT_CONTRACT_0_realloc(&tx, call_pages, tx.user_data), 
                    (_,1) => DEFAULT_CONTRACT_1_assign(&tx, call_pages, tx.user_data), 
                    // contract methods
                    (DEFAULT_CONTRACT,128) => DEFAULT_CONTRACT_1_move_funds(&tx, call_pages, tx.user_data), 
                    (contract,method) => warn!("unknown contract and method {:x} {:x}", contract,method),
                };

		        // TODO(anatoly): Verify Memory
                // Pages owned by the contract are Read/Write,
                // pages not owned by the contract are
		        // Read only.  Code should verify memory integrity or
		        // verify contract bytecode.
                
                // verify tokens
                let after_call_total_spendable = call_pages.map(|(page,proof)| {
                    if page.contract == tx.contract {
                        page.balance;
                    }
                }).sum();
                let after_call_total_unspendable = call_pages.map(|page| {
                    if page.contract != tx.contract {
                        page.balance;
                    }
                }).sum();  
                let after_call_total = after_call_total_spendable + after_call_total_unspendable;

                //commit
                if after_call_total == pre_call_total {
                    if after_call_total_spendable == pre_call_total_spendable {
                        loaded_pages.zip(call_pages).map(|load,call| {
                            *load = call;
                        });
                    }
                }
            }
        });
    }

    /// parallel execution of contracts
    /// first we load the pages, then we pass all the pages to `par_execute` function which can
    /// safely call them all in parallel
    pub fn load_and_execute(&mut self, 
        packet: &Vec<Call>,
        from_pages: &Vec<Option<usize>>,
        to_pages: &Vec<Vec<usize>>,
        load_pages: &mut Vec<Option<(Vec<&mut Page>)>>,
    ) {
        let allocated_pages = self.allocated_pages.read().unwrap();
        self.load_pages(&allocated_pages, packet, from_pages, to_pages, load_pages); 
        Self::par_execute(&allocated_pages, packet, load_pages); 
    }

    pub fn get_balance(&self, key: &PublicKey) -> Option<u64> {
        //todo does this hold the lock through the map?
        self.allocated_pages
            .read()
            .unwrap()
            .lookup(key)
            .map(|dx| self.page_table[dx].balance)
    }
    pub fn get_version(&self, key: &PublicKey) -> Option<u64> {
        //todo does this hold the lock through the map?
        self.allocated_pages
            .read()
            .unwrap()
            .lookup(key)
            .map(|dx| self.page_table[dx].version)
    }
    fn _load_memory(
        &self,
        _packet: &Vec<Call>,
        _from_pages: &Vec<Option<usize>>,
        _to_pages: &Vec<Option<usize>>,
    ) {
        //TBD
    }
    fn _execute_call(
        &self,
        _packet: &Vec<Call>,
        _from_pages: &Vec<Option<usize>>,
        _to_pages: &Vec<Option<usize>>,
    ) {
        //TBD
    }
    fn _write_memory(
        &self,
        _packet: &Vec<Call>,
        _from_pages: &Vec<Option<usize>>,
        _to_pages: &Vec<Option<usize>>,
    ) {
        //TBD
    }
    pub fn release_memory_lock(&self, packet: &Vec<Call>, lock: &Vec<bool>) {
        //holds mem_locks mutex
        let mut mem_locks = self.mem_locks.lock().unwrap();
        for (i, p) in packet.iter().enumerate() {
            if !lock[i] {
                continue;
            }
            mem_locks.remove(&p.call.caller);
            mem_locks.remove(&p.destination);
        }
    }
    /// fill up to the blob
    pub fn fill_blob(
        packet: &Vec<Call>,
        from_pages: &Vec<Option<&Page>>,
        to_pages: &Vec<Option<&Page>>,
        blob: &mut Vec<Call>,
        filled: &mut usize,
    ) {
        //nolock
        for (i, tx) in packet.iter().enumerate() {
            if from_pages[i].is_none() || to_pages[i].is_none() {
                continue;
            }
            blob[*filled] = tx.clone();
            *filled += 1;
            if *filled == blob.len() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use logger;
    use page_table::{Call, PageTable, Tx};
    use rand;
    use rand::RngCore;
    const N: usize = 2;

    fn rand4() -> [u64; 4] {
        let mut r = rand::thread_rng();
        [r.next_u64(), r.next_u64(), r.next_u64(), r.next_u64()]
    }
    fn random_tx() -> Tx {
        assert_ne!(rand4(), rand4());
        Tx {
            call: Call {
                signature: [0; 8],
                owner: rand4(),
                contract: rand4(),
                version: 1,
                amount: 1,
                fee: 1,
                method: 0,
                inkeys: 1,
                num_user_data: 0,
                num_spends: 1,
                num_proofs: 0,
                unused: [0u8; 3],
            },
            destination: rand4(),
        }
    }
    #[test]
    fn mem_lock() {
        let pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        let mut lock = vec![false; N];
        let mut lock2 = vec![false; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        for x in &lock {
            assert!(*x);
        }
        pt.acquire_memory_lock(&transactions, &mut lock2);
        for x in &lock2 {
            assert!(!*x);
        }
        pt.release_memory_lock(&transactions, &lock);

        pt.acquire_memory_lock(&transactions, &mut lock2);
        for x in &lock2 {
            assert!(*x);
        }
        pt.release_memory_lock(&transactions, &lock2);
    }
    #[test]
    fn validate_debits_miss() {
        let mut pt = PageTable::new();
        let fill_table: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&fill_table, true, 1_000_000);
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        for x in &from_pages {
            assert!(x.is_none());
        }
    }
    #[test]
    fn validate_debits_hit() {
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        for x in &from_pages {
            assert!(x.is_some());
        }
    }
    #[test]
    fn validate_debits_low_version() {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        for tx in &mut transactions {
            tx.call.version = 0;
        }
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        for x in &from_pages {
            assert!(x.is_none());
        }
    }
    #[test]
    fn find_new_keys_all() {
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        let mut to_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
        for x in &to_pages {
            assert!(x.is_none());
        }
    }
    #[test]
    fn find_new_keys_none() {
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        pt.force_allocate(&transactions, false, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        let mut to_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
        for x in &to_pages {
            assert!(x.is_some());
        }
    }
    #[test]
    fn allocate_new_keys_all() {
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        let mut to_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
        pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
        for x in &to_pages {
            assert!(x.is_some());
        }
    }
    #[test]
    fn allocate_new_keys_none() {
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        pt.force_allocate(&transactions, false, 1_000_000);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        let mut to_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
        pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
        for x in &to_pages {
            assert!(x.is_some());
        }
    }
    #[test]
    fn move_funds() {
        logger::setup();
        let mut pt = PageTable::new();
        let transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 10);
        let mut lock = vec![false; N];
        let mut from_pages = vec![None; N];
        let mut to_pages = vec![None; N];
        pt.acquire_memory_lock(&transactions, &mut lock);
        pt.validate_debits(&transactions, &lock, &mut from_pages);
        pt.check_pages(&transactions, &from_pages, &to_pages);
        pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
        pt.check_pages(&transactions, &from_pages, &to_pages);
        pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
        pt.check_pages(&transactions, &from_pages, &to_pages);
        pt.move_funds(&transactions, &mut from_pages, &mut to_pages);
        pt.check_pages(&transactions, &from_pages, &to_pages);
        for (i, x) in transactions.iter().enumerate() {
            assert!(to_pages[i].is_some());
            assert!(from_pages[i].is_some());
            assert_eq!(pt.get_balance(&x.destination), Some(x.call.amount));
            assert_eq!(pt.get_version(&x.call.caller), Some(x.call.version));
            assert_eq!(
                pt.get_balance(&x.call.caller),
                Some(10 - (x.call.amount + x.call.fee))
            );
        }
    }
}

#[cfg(all(feature = "unstable", test))]
mod bench {
    extern crate test;
    use self::test::Bencher;
    use page_table::{Call, PageTable, Tx};
    use rand;
    use rand::RngCore;
    const N: usize = 256;
    fn rand4() -> [u64; 4] {
        let mut r = rand::thread_rng();
        [r.next_u64(), r.next_u64(), r.next_u64(), r.next_u64()]
    }
    fn random_tx() -> Tx {
        assert_ne!(rand4(), rand4());
        Tx {
            call: Call {
                signature: [0; 8],
                owner: rand4(),
                contract: rand4(),
                version: 1,
                amount: 1,
                fee: 1,
                method: 0,
                inkeys: 1,
                num_user_data: 0,
                num_spends: 1,
                num_proofs: 0,
                unused: [0u8; 3],
            },
            destination: rand4(),
        }
    }
    #[bench]
    fn bench_update_version(bencher: &mut Bencher) {
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        bencher.iter(move || {
            for tx in &mut transactions {
                tx.call.version += 1;
            }
        });
    }
    #[bench]
    fn bench_mem_lock(bencher: &mut Bencher) {
        let pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        bencher.iter(move || {
            let mut lock = vec![false; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_validate_debits_miss(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let fill_table: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&fill_table, true, 1_000_000);
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_validate_debits_hit(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_find_new_keys_all(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_find_new_keys_none(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        pt.force_allocate(&transactions, false, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_allocate_new_keys_all(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
            pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_allocate_new_keys_none(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        pt.force_allocate(&transactions, false, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
            pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_move_funds(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut transactions: Vec<_> = (0..N).map(|_r| random_tx()).collect();
        pt.force_allocate(&transactions, true, 1_000_000);
        bencher.iter(move || {
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in &mut transactions {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(&transactions, &mut lock);
            pt.validate_debits(&transactions, &lock, &mut from_pages);
            pt.find_new_keys(&transactions, &from_pages, &mut to_pages);
            pt.allocate_keys(&transactions, &from_pages, &mut to_pages);
            pt.move_funds(&transactions, &mut from_pages, &mut to_pages);
            pt.release_memory_lock(&transactions, &lock);
        });
    }
    #[bench]
    fn bench_move_funds_large_table(bencher: &mut Bencher) {
        let mut pt = PageTable::new();
        let mut ttx: Vec<Vec<_>> = (0..N)
            .map(|_| (0..N).map(|_r| random_tx()).collect())
            .collect();
        for transactions in &ttx {
            pt.force_allocate(transactions, true, 1_000_000);
        }
        bencher.iter(move || {
            let transactions = &mut ttx[rand::thread_rng().next_u64() as usize % N];
            let mut lock = vec![false; N];
            let mut from_pages = vec![None; N];
            let mut to_pages = vec![None; N];
            for tx in transactions.iter_mut() {
                tx.call.version += 1;
            }
            pt.acquire_memory_lock(transactions, &mut lock);
            pt.validate_debits(transactions, &lock, &mut from_pages);
            pt.find_new_keys(transactions, &from_pages, &mut to_pages);
            pt.allocate_keys(transactions, &from_pages, &mut to_pages);
            pt.move_funds(transactions, &mut from_pages, &mut to_pages);
            pt.release_memory_lock(transactions, &lock);
        });
    }
}
