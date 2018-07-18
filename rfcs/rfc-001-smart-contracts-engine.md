# Smart Contracts Engine
 
The goal of this RFC is to define a set of constraints for APIs and runtime such that we can execute our smart contracts safely on massively parallel hardware such as a GPU.  Our runtime is built around an OS *syscall* primitive.  The difference in blockchain is that now the OS does a cryptographic check of memory region ownership before accessing the memory in the Solana kernel.

## Version

version 0.2 

## Toolchain Stack

     +---------------------+       +---------------------+
     |                     |       |                     |
     |   +------------+    |       |   +------------+    |
     |   |            |    |       |   |            |    |
     |   |  frontend  |    |       |   |  verifier  |    |
     |   |            |    |       |   |            |    |
     |   +-----+------+    |       |   +-----+------+    |
     |         |           |       |         |           |
     |         |           |       |         |           |
     |   +-----+------+    |       |   +-----+------+    |
     |   |            |    |       |   |            |    |
     |   |    llvm    |    |       |   |   loader   |    |
     |   |            |    +------>+   |            |    |
     |   +-----+------+    |       |   +-----+------+    |
     |         |           |       |         |           |
     |         |           |       |         |           |
     |   +-----+------+    |       |   +-----+------+    |
     |   |            |    |       |   |            |    |
     |   |    ELF     |    |       |   |   runtime  |    |
     |   |            |    |       |   |            |    |
     |   +------------+    |       |   +------------+    |
     |                     |       |                     |
     |        client       |       |       solana        |
     +---------------------+       +---------------------+

                [Figure 1. Smart Contracts Stack]

In Figure 1 an untrusted client, creates a program in the front-end language of her choice, (like C/C++/Rust/Lua), and compiles it with LLVM to a position independent shared object ELF, targeting BPF bytecode. Solana will safely load and execute the ELF.

## Runtime

The goal with the runtime is to have a general purpose execution environment that is highly parallelizeable and doesn't require dynamic resource management. The goal is to execute as many contracts as possible in parallel, and have them pass or fail without a destructive state change.


### State

State is addressed by an account which is at the moment simply the PubKey.  Our goal is to eliminate dynamic memory allocation in the smart contract itself, so the contract is a function that takes a mapping of [(PubKey,State)] and returns [(PubKey, State')].

### Call Structure
```
pub struct Call {
    /// Signatures and Keys
    /// proofs[0] is the signature
    /// number of proofs of ownership of `inkeys`, `owner` is proven by the signature
    proofs: Vec<Option<Signature>>,
    /// number of keys to load, aka the to key
    /// inkeys[0] is the caller's key
    keys: Vec<PublicKey>,

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
    pub version: u64,
    /// method to call in the contract
    method: u8,
    /// usedata in bytes
    user_data: Vec<u8>,
}
```


At it's core, this is just a set of PublicKeys and Signatures with a bit of metadata.  The contract PublicKey routes this transaction into that contracts entry point.  `version` is used for dropping retransmitted requests.

Contracts should be able to read any state that is part of solana, but only write to state that the contract allocated.

### Execution

Calls batched and processed in a pipeline

```
+-----------+    +-------------+    +--------------+    +---------------+    
| sigverify |--->| lock memory |--->| validate fee |--->| allocate keys |--->
+-----------+    +-------------+    +--------------+    +---------------+    
                                
    +------------+    +---------+    +--------------+    +-=------------+   
--->| load pages |--->| execute |--->|unlock memory |--->| commit pages |   
    +------------+    +---------+    +--------------+    +--------------+   

```

At the `execute` stage, the loaded pages have no data dependencies, so all the contracts can be executed in parallel. 
## Memory Management
```
pub struct Page {
    /// key that indexes this page
    /// proove ownership of this key to spend from this Page
    owner: PublicKey,
    /// contract that owns this page
    /// contract can write to the data that is pointed to by `pointer`
    contract: PublicKey,
    /// balance that belongs to owner
    balance: u64,
    /// version of the structure, public for testing
    version: u64,
    /// hash of the page data
    memhash: Hash,
    /// The following could be in a separate structure
    memory: Vec<u8>,
}
```

The guarantee that solana enforces:
    1. The contract code is the only code that will modify the contents of `memory`
    2. Total balances on all the pages is equal before and after exectuion of a call
    3. Balances of each of the pages not owned by the contract must be equal to or greater after the call than before the call.

## Entry Point
Exectuion of the contract involves maping the contract's public key to an entry point which takes a pointer to the transaction, and an array of loaded pages.
```
// Find the method
match (tx.contract, tx.method) {
    // system interface
    // everyone has the same reallocate
    (_, 0) => system_0_realloc(&tx, &mut call_pages),
    (_, 1) => system_1_assign(&tx, &mut call_pages),
    // contract methods
    (DEFAULT_CONTRACT, 128) => default_contract_128_move_funds(&tx, &mut call_pages),
    (contract, method) => //... 
```

The first 127 methods are reserved for the system interface, which implements allocation and assignment of memory.  The rest, including the contract for moving funds are implemented by the contract itself.

## System Interface
```
/// SYSTEM interface, same for very contract, methods 0 to 127
/// method 0
/// reallocate
/// spend the funds from the call to the first recepient
pub fn system_0_realloc(call: &Call, pages: &mut Vec<Page>) {
    if call.contract == DEFAULT_CONTRACT {
        let size: u64 = deserialize(&call.user_data).unwrap();
        pages[0].memory.resize(size as usize, 0u8);
    }
}
/// method 1
/// assign
/// assign the page to a contract
pub fn system_1_assign(call: &Call, pages: &mut Vec<Page>) {
    let contract = deserialize(&call.user_data).unwrap();
    if call.contract == DEFAULT_CONTRACT {
        pages[0].contract = contract;
        //zero out the memory in pages[0].memory
        //Contracts need to own the state of that data otherwise a use could fabricate the state and
        //manipulate the contract
        pages[0].memory.clear();
    }
} 
```
The first method resizes the memory that is assosciated with the callers page.  The second system call assignes the page to the contract.  Both methods check if the current contract is 0, otherwise the method does nothing and the caller spent their fees.

This ensures that when memory is assigned to the contract the initial state of all the bytes is 0, and the contract itself is the only thing that can modify that state.

## Simplest contract
```
/// DEFAULT_CONTRACT interface
/// All contracts start with 128
/// method 128
/// move_funds
/// spend the funds from the call to the first recepient
pub fn default_contract_128_move_funds(call: &Call, pages: &mut Vec<Page>) {
    let amount: u64 = deserialize(&call.user_data).unwrap();
    if pages[0].balance >= amount  {
        pages[0].balance -= amount;
        pages[1].balance += amount;
    }
}
``` 

This simply moves the amount from page[0], which is the callers page, to page[1], which is the recipients page.

## Notes

1. There is no dynamic memory allocation.
2. Persistent Memory is allocated to a Key with ownership
3. Contracts can `call` to update key owned state
4. `call` is just a *syscall* that does a cryptographic check of memory ownership
5. Kernel guarantees that when memory is assigned to the contract its state is 0
6. Kernel guarantees that contract is the only thing that can modify memory that its assigned to
7. Kernel guarantees that the contract can only spend tokens that are in pages that are assigned to it
8. Kernel guarantees the balances belonging to pages are balanced before and after the call
