use bigint::U256;
use rand::random;
use sha3::{Digest, Sha3_256};

use core::str::FromStr;
use std::env;
use std::fmt::{self, Display};
use std::net::{SocketAddr, AddrParseError};

use cryptocurrency_kit::crypto::{hash, CryptoHash, Hash};
use cryptocurrency_kit::merkle_tree::MerkleTree;
use cryptocurrency_kit::storage::values::StorageValue;
use libp2p::{
    multiaddr::Protocol,
    Multiaddr,
};

pub fn merkle_tree_root<T: StorageValue>(input: Vec<T>) -> Hash {
    let mut v: Vec<Vec<_>> = vec![];
    for item in input {
        let bytes = item.into_bytes();
        v.push(bytes);
    }
    let root = MerkleTree::new_merkle_tree(v).root.unwrap();
//    info!("{:?}, len: {}", &root.data);
    Hash::from_slice(&root.data).unwrap()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HexBytes {
    inner: [u8; 32],
}

impl HexBytes {
    pub fn bytes(&self) -> &[u8; 32] {
        &self.inner
    }

    pub fn string(&self) -> String {
        String::from_utf8_lossy(&self.inner).to_string()
    }
}

impl Display for HexBytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use std::fmt;
        writeln!(f, "{}", self.string()).unwrap();
        Ok(())
    }
}

pub fn as_256(data: &[u8]) -> U256 {
    U256::from_big_endian(data)
}

pub fn u256_hash(input: &[u8]) -> Vec<u8> {
    let mut hasher = Sha3_256::default();
    hasher.input(input);
    hasher.result().to_vec()
}

pub fn random_dir() -> Box<String> {
    Box::new(format!(
        "{}{}",
        env::temp_dir().to_str().unwrap(),
        random::<u64>()
    ))
}


pub fn multiaddr_to_ipv4(mul_addr: &Multiaddr) -> Result<SocketAddr, AddrParseError> {
    let mut ipv4: String = "".to_string();
    let v = mul_addr.iter().collect::<Vec<_>>();
    for protocol in v {
        match protocol {
            Protocol::Ip4(ref ip4) => {
                ipv4.push_str(&format!("{}:", ip4));
            }
            Protocol::Tcp(ref port) => {
                ipv4.push_str(&format!("{}", port));
            }
            _ => {}
        }
    }
    ipv4.parse()
}

pub fn random_uuid() -> uuid::Uuid {
    use uuid::Uuid;
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, chrono::Local::now().to_string().as_bytes())
}


use ethereum_types::{Address, H160};

pub fn string_to_address(s: &String) -> Result<Address, String> {
    if s.len() < 40 {
        return Err("less than 40 chars".to_string());
    }
    if s.len() > 42 {
        return Err("more than 42 chars".to_string());
    }

    if s.len() == 42 {
        return Ok(Address::from_str(&s[2..]).unwrap());
    }
    Ok(Address::from_str(&s).unwrap())
}

pub fn strings_to_addresses(strs: &Vec<String>) -> Result<Vec<Address>, String> {
    let mut addresses = Vec::new();
    for str in strs {
        let address = string_to_address(str)?;
        addresses.push(address);
    }
    Ok(addresses)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn t_string_to_address() {
        let address = string_to_address(&"0x93908f59c6eff007d228398349214acb6b4ac9a4".to_owned()).unwrap();
        assert_eq!("0x93908f59c6eff007d228398349214acb6b4ac9a4", format!("{:?}", address));
        println!("address: {:?}", address);
    }
}