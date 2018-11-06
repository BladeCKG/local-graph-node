extern crate failure;
extern crate graph_mock;
extern crate graphql_parser;
extern crate ipfs_api;
extern crate parity_wasm;

use self::graph_mock::FakeStore;
use ethabi::{LogParam, Token};
use failure::Error;
use futures::sync::mpsc::{channel, Receiver, Sender};
use graph::components::ethereum::*;
use graph::components::store::*;
use graph::components::subgraph::*;
use graph::data::store::scalar;
use graph::data::subgraph::*;
use graph::util;
use graph::web3::types::{Address, H256, U256};
use hex;
use std::collections::HashMap;
use std::io::Cursor;
use std::iter::FromIterator;
use std::str::FromStr;
use std::sync::Mutex;

use super::{Error as WasmiError, *};

use self::graphql_parser::schema::Document;

#[derive(Default)]
struct MockEthereumAdapter {}

impl EthereumAdapter for MockEthereumAdapter {
    fn net_identifiers(
        &self,
        _: &Logger,
    ) -> Box<Future<Item = EthereumNetworkIdentifier, Error = Error> + Send> {
        unimplemented!();
    }

    fn block_by_hash(
        &self,
        _: &Logger,
        block_hash: H256,
    ) -> Box<Future<Item = Option<EthereumBlock>, Error = Error> + Send> {
        unimplemented!();
    }

    fn block_hash_by_block_number(
        &self,
        _: &Logger,
        block_number: u64,
    ) -> Box<Future<Item = Option<H256>, Error = Error> + Send> {
        unimplemented!();
    }

    fn is_on_main_chain(
        &self,
        _: &Logger,
        block_ptr: EthereumBlockPointer,
    ) -> Box<Future<Item = bool, Error = Error> + Send> {
        unimplemented!();
    }

    fn find_first_blocks_with_logs(
        &self,
        _: &Logger,
        from: u64,
        to: u64,
        log_filter: EthereumLogFilter,
    ) -> Box<Future<Item = Vec<EthereumBlockPointer>, Error = Error> + Send> {
        unimplemented!();
    }

    fn contract_call(
        &self,
        _: &Logger,
        call: EthereumContractCall,
    ) -> Box<Future<Item = Vec<Token>, Error = EthereumContractCallError> + Send> {
        unimplemented!();
    }
}

fn test_module(
    data_source: DataSource,
) -> (WasmiModule<
    MockEthereumAdapter,
    ipfs_api::IpfsClient,
    FakeStore,
    Sender<Box<Future<Item = (), Error = ()> + Send>>,
>) {
    let logger = slog::Logger::root(slog::Discard, o!());
    let mock_ethereum_adapter = Arc::new(MockEthereumAdapter::default());
    let (task_sender, task_receiver) = channel(100);
    let mut runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.spawn(task_receiver.for_each(tokio::spawn));
    ::std::mem::forget(runtime);
    WasmiModule::new(
        &logger,
        WasmiModuleConfig {
            subgraph: mock_subgraph(),
            data_source,
            ethereum_adapter: mock_ethereum_adapter,
            link_resolver: Arc::new(ipfs_api::IpfsClient::default()),
            store: Arc::new(FakeStore),
        },
        task_sender,
    )
}

fn mock_subgraph() -> SubgraphManifest {
    SubgraphManifest {
        id: String::from("example subgraph"),
        location: String::from("/path/to/example-subgraph.yaml"),
        spec_version: String::from("0.1.0"),
        schema: Schema {
            id: String::from("exampled id"),
            document: Document {
                definitions: vec![],
            },
        },
        data_sources: vec![],
    }
}

fn mock_data_source(path: &str) -> DataSource {
    let runtime = parity_wasm::deserialize_file(path).expect("Failed to deserialize wasm");

    DataSource {
        kind: String::from("ethereum/contract"),
        name: String::from("example data source"),
        source: Source {
            address: Address::from_str("0123123123012312312301231231230123123123").unwrap(),
            abi: String::from("123123"),
        },
        mapping: Mapping {
            kind: String::from("ethereum/events"),
            api_version: String::from("0.1.0"),
            language: String::from("wasm/assemblyscript"),
            entities: vec![],
            abis: vec![],
            event_handlers: vec![],
            runtime,
        },
    }
}

impl<T, L, S, U> WasmiModule<T, L, S, U>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store + Send + Sync + 'static,
    U: Sink<SinkItem = Box<Future<Item = (), Error = ()> + Send>> + Clone + 'static,
{
    fn takes_val_returns_ptr<P>(&mut self, fn_name: &str, val: RuntimeValue) -> AscPtr<P> {
        self.module
            .invoke_export(fn_name, &[val], &mut self.externals)
            .expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return pointer")
    }
}

#[cfg(any())]
#[test]
fn call_invalid_event_handler_and_dont_crash() {
    // This test passing means the module doesn't crash when an invalid
    // event handler is called or when the event handler execution fails.

    let (mut module, _) = test_module(mock_data_source("wasm_test/example_event_handler.wasm"));

    // Create a mock Ethereum event
    let ethereum_event = EthereumEvent {
        address: Address::from("22843e74c59580b3eaf6c233fa67d8b7c561a835"),
        event_signature: util::ethereum::string_to_h256("ExampleEvent(string)"),
        block_hash: util::ethereum::string_to_h256("example block hash"),
        params: vec![LogParam {
            name: String::from("exampleParam"),
            value: Token::String(String::from("some data")),
        }],
        removed: false,
    };

    // Call a non-existent event handler in the test module; if the test hasn't
    // crashed until now, it means it survives Ethereum event handler errors
    assert_eq!(
        module.handle_ethereum_event("handleNonExistentExampleEvent", ethereum_event),
        ()
    );
}

#[cfg(any())]
#[test]
fn call_event_handler_and_receive_store_event() {
    // Load the example_event_handler.wasm test module. All this module does
    // is implement an `handleExampleEvent` function that calls `store.set()`
    // with sample data taken from the event parameters.
    //
    // This test verifies that the event is delivered and the example data
    // is returned to the RuntimeHostEvent stream.

    let (mut module, receiver) =
        test_module(mock_data_source("wasm_test/example_event_handler.wasm"));

    // Create a mock Ethereum event
    let ethereum_event = EthereumEvent {
        address: Address::from("22843e74c59580b3eaf6c233fa67d8b7c561a835"),
        event_signature: util::ethereum::string_to_h256("ExampleEvent(string)"),
        block_hash: util::ethereum::string_to_h256("example block hash"),
        params: vec![LogParam {
            name: String::from("exampleParam"),
            value: Token::String(String::from("some data")),
        }],
        removed: false,
    };

    // Call the event handler in the test module and pass the event to it
    module.handle_ethereum_event("handleExampleEvent", ethereum_event);

    // Expect a store set call to be made by the handler and a
    // RuntimeHostEvent::EntitySet event to be written to the event stream
    let work = receiver.take(1).into_future();
    let store_event = work
        .wait()
        .expect("No store event received from runtime")
        .0
        .expect("Store event must not be None");

    // Verify that this event matches what the test module is sending
    assert_eq!(
        store_event,
        RuntimeHostEvent::EntitySet(
            StoreKey {
                subgraph: String::from("example subgraph"),
                entity: String::from("ExampleEntity"),
                id: String::from("example id"),
            },
            Entity::from(HashMap::from_iter(
                vec![(String::from("exampleAttribute"), Value::from("some data"))].into_iter()
            )),
            EventSource::EthereumBlock(util::ethereum::string_to_h256("example block hash",)),
        )
    );
}

#[test]
fn json_conversions() {
    let mut module = test_module(mock_data_source("wasm_test/string_to_number.wasm"));

    // test u64 conversion
    let number = 9223372036850770800;
    let converted: u64 = module
        .module
        .invoke_export(
            "testToU64",
            &[RuntimeValue::from(module.heap.asc_new(&number.to_string()))],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return I64");
    assert_eq!(number, converted);

    // test i64 conversion
    let number = -9223372036850770800;
    let converted: i64 = module
        .module
        .invoke_export(
            "testToI64",
            &[RuntimeValue::from(module.heap.asc_new(&number.to_string()))],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return I64");
    assert_eq!(number, converted);

    // test f64 conversion
    let number = F64::from(-9223372036850770.92345034);
    let converted: F64 = module
        .module
        .invoke_export(
            "testToF64",
            &[RuntimeValue::from(
                module.heap.asc_new(&number.to_float().to_string()),
            )],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return F64");
    assert_eq!(number, converted);

    // test BigInt conversion
    let number = "-922337203685077092345034";
    let big_int_obj: AscPtr<AscBigInt> = module
        .module
        .invoke_export(
            "testToBigInt",
            &[RuntimeValue::from(module.heap.asc_new(number))],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let bytes: Vec<u8> = module.heap.asc_get(big_int_obj);
    assert_eq!(
        scalar::BigInt::from_str(number).unwrap(),
        scalar::BigInt::from_signed_bytes_le(&bytes)
    );
}

#[test]
fn ipfs_cat() {
    let mut module = test_module(mock_data_source("wasm_test/ipfs_cat.wasm"));
    let ipfs = Arc::new(ipfs_api::IpfsClient::default());

    let mut runtime = tokio::runtime::Runtime::new().unwrap();
    let hash = runtime.block_on(ipfs.add(Cursor::new("42"))).unwrap().hash;
    let converted: AscPtr<AscString> = module
        .module
        .invoke_export(
            "ipfsCat",
            &[RuntimeValue::from(module.heap.asc_new(&hash))],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let data: String = module.heap.asc_get(converted);
    assert_eq!(data, "42");
}

#[test]
fn crypto_keccak256() {
    let mut module = test_module(mock_data_source("wasm_test/crypto.wasm"));
    let input: &[u8] = "eth".as_ref();
    let input: AscPtr<Uint8Array> = module.heap.asc_new(input);

    let hash: AscPtr<Uint8Array> = module
        .module
        .invoke_export("hash", &[RuntimeValue::from(input)], &mut module.externals)
        .expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let hash: Vec<u8> = module.heap.asc_get(hash);
    assert_eq!(
        hex::encode(hash),
        "4f5b812789fc606be1b3b16908db13fc7a9adf7ca72641f84d75b47069d3d7f0"
    );
}

#[test]
fn token_numeric_conversion() {
    let mut module = test_module(mock_data_source("wasm_test/token_to_numeric.wasm"));

    // Convert numeric to token and back.
    let num = i32::min_value();
    let token_ptr: AscPtr<AscEnum<EthereumValueKind>> =
        module.takes_val_returns_ptr("token_from_i32", RuntimeValue::from(num));
    let num_return = module
        .module
        .invoke_export(
            "token_to_i32",
            &[RuntimeValue::from(token_ptr)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into::<i32>()
        .expect("call did not return i32");
    assert_eq!(num, num_return);
}

#[test]
fn big_int_to_from_i32() {
    let mut module = test_module(mock_data_source("wasm_test/big_int_to_from_i32.wasm"));

    // Convert i32 to BigInt
    let input: i32 = -157;
    let output_ptr: AscPtr<AscBigInt> = module
        .module
        .invoke_export(
            "i32_to_big_int",
            &[RuntimeValue::from(input)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let output: BigInt = module.heap.asc_get(output_ptr);
    assert_eq!(output, BigInt::from(-157 as i32));

    // Convert BigInt to i32
    let input = BigInt::from(-50 as i32);
    let input_ptr: AscPtr<AscBigInt> = module.heap.asc_new(&input);
    let output: i32 = module
        .module
        .invoke_export(
            "big_int_to_i32",
            &[RuntimeValue::from(input_ptr)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    assert_eq!(output, -50 as i32);
}

#[test]
fn big_int_to_hex() {
    let mut module = test_module(mock_data_source("wasm_test/big_int_to_hex.wasm"));

    // Convert zero to hex
    let zero = BigInt::from_unsigned_u256(&U256::zero());
    let zero: AscPtr<AscBigInt> = module.heap.asc_new(&zero);
    let zero_hex_ptr: AscPtr<AscString> = module
        .module
        .invoke_export(
            "big_int_to_hex",
            &[RuntimeValue::from(zero)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let zero_hex_str: String = module.heap.asc_get(zero_hex_ptr);
    assert_eq!(zero_hex_str, "0x0");

    // Convert 1 to hex
    let one = BigInt::from_unsigned_u256(&U256::one());
    let one: AscPtr<AscBigInt> = module.heap.asc_new(&one);
    let one_hex_ptr: AscPtr<AscString> = module
        .module
        .invoke_export(
            "big_int_to_hex",
            &[RuntimeValue::from(one)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let one_hex_str: String = module.heap.asc_get(one_hex_ptr);
    assert_eq!(one_hex_str, "0x1");

    // Convert U256::max_value() to hex
    let u256_max = BigInt::from_unsigned_u256(&U256::max_value());
    let u256_max: AscPtr<AscBigInt> = module.heap.asc_new(&u256_max);
    let u256_max_hex_ptr: AscPtr<AscString> = module
        .module
        .invoke_export(
            "big_int_to_hex",
            &[RuntimeValue::from(u256_max)],
            &mut module.externals,
        ).expect("call failed")
        .expect("call returned nothing")
        .try_into()
        .expect("call did not return pointer");
    let u256_max_hex_str: String = module.heap.asc_get(u256_max_hex_ptr);
    assert_eq!(
        u256_max_hex_str,
        "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    );
}

#[test]
fn abort() {
    let mut module = test_module(mock_data_source("wasm_test/abort.wasm"));
    let err = module
        .module
        .invoke_export("abort", &[], &mut module.externals)
        .unwrap_err();
    assert_eq!(err.to_string(), "Trap: Trap { kind: Host(HostExportError(\"Mapping aborted at abort.ts, line 1, column 1, with message: `aborted`\")) }");
}
