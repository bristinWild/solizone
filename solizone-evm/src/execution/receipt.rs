use revm::{
    context::result::ExecutionResult,
    primitives::{Address, B256, Bytes, keccak256},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    Success,
    Revert,
    Halt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLog {
    /// Contract that emitted the log.
    pub address: Address,

    /// Event signature and indexed parameters.
    pub topics: Vec<B256>,

    /// ABI-encoded non-indexed event data.
    pub data: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReceipt {
    /// High-level result of execution.
    pub status: ExecutionStatus,

    /// EVM transaction gas consumed.
    pub gas_used: u64,

    /// Return or revert data produced by execution.
    pub output: Bytes,

    /// Address created by a CREATE transaction.
    ///
    /// None for normal calls and transfers.
    pub contract_address: Option<Address>,

    pub logs: Vec<ExecutionLog>,
}

const RECEIPT_MAGIC: &[u8; 4] = b"SZR1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptDecodeError {
    UnexpectedEof,
    InvalidMagic,
    InvalidStatus(u8),
    InvalidOptionTag(u8),
    TrailingBytes,
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], ReceiptDecodeError> {
    if input.len() < len {
        return Err(ReceiptDecodeError::UnexpectedEof);
    }

    let (head, tail) = input.split_at(len);
    *input = tail;

    Ok(head)
}

fn take_u32(input: &mut &[u8]) -> Result<usize, ReceiptDecodeError> {
    let bytes: [u8; 4] = take(input, 4)?.try_into().expect("4 bytes");

    Ok(u32::from_be_bytes(bytes) as usize)
}

impl ExecutionStatus {
    fn as_byte(&self) -> u8 {
        match self {
            ExecutionStatus::Success => 0,
            ExecutionStatus::Revert => 1,
            ExecutionStatus::Halt => 2,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, ReceiptDecodeError> {
        match byte {
            0 => Ok(ExecutionStatus::Success),
            1 => Ok(ExecutionStatus::Revert),
            2 => Ok(ExecutionStatus::Halt),
            other => Err(ReceiptDecodeError::InvalidStatus(other)),
        }
    }
}

impl ExecutionReceipt {
    pub fn from_revm(result: &ExecutionResult) -> Self {
        let status = match result {
            ExecutionResult::Success { .. } => ExecutionStatus::Success,
            ExecutionResult::Revert { .. } => ExecutionStatus::Revert,
            ExecutionResult::Halt { .. } => ExecutionStatus::Halt,
        };

        let logs = result
            .logs()
            .iter()
            .map(|log| ExecutionLog {
                address: log.address,
                topics: log.data.topics().to_vec(),
                data: log.data.data.clone(),
            })
            .collect();

        Self {
            status,
            gas_used: result.tx_gas_used(),
            output: result.output().cloned().unwrap_or_default(),
            contract_address: result.created_address(),
            logs,
        }
    }

    pub fn encode_canonical(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        // Solizone Receipt V1
        bytes.extend_from_slice(RECEIPT_MAGIC);

        // Status
        bytes.push(self.status.as_byte());

        // Gas
        bytes.extend_from_slice(&self.gas_used.to_be_bytes());

        // Optional created contract address
        match self.contract_address {
            Some(address) => {
                bytes.push(1);
                bytes.extend_from_slice(address.as_slice());
            }
            None => {
                bytes.push(0);
            }
        }

        // Output
        let output_len = u32::try_from(self.output.len()).expect("receipt output too large");

        bytes.extend_from_slice(&output_len.to_be_bytes());
        bytes.extend_from_slice(self.output.as_ref());

        // Logs
        let log_count = u32::try_from(self.logs.len()).expect("too many receipt logs");

        bytes.extend_from_slice(&log_count.to_be_bytes());

        for log in &self.logs {
            bytes.extend_from_slice(log.address.as_slice());

            let topic_count = u8::try_from(log.topics.len()).expect("too many log topics");

            bytes.push(topic_count);

            for topic in &log.topics {
                bytes.extend_from_slice(topic.as_slice());
            }

            let data_len = u32::try_from(log.data.len()).expect("log data too large");

            bytes.extend_from_slice(&data_len.to_be_bytes());
            bytes.extend_from_slice(log.data.as_ref());
        }

        bytes
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, ReceiptDecodeError> {
        let mut input = bytes;

        if take(&mut input, 4)? != RECEIPT_MAGIC {
            return Err(ReceiptDecodeError::InvalidMagic);
        }

        let status = ExecutionStatus::from_byte(take(&mut input, 1)?[0])?;

        let gas_used = u64::from_be_bytes(take(&mut input, 8)?.try_into().expect("8 bytes"));

        let contract_address = match take(&mut input, 1)?[0] {
            0 => None,
            1 => Some(Address::from_slice(take(&mut input, 20)?)),
            other => return Err(ReceiptDecodeError::InvalidOptionTag(other)),
        };

        let output_len = take_u32(&mut input)?;
        let output = Bytes::copy_from_slice(take(&mut input, output_len)?);

        let log_count = take_u32(&mut input)?;
        let mut logs = Vec::new();

        for _ in 0..log_count {
            let address = Address::from_slice(take(&mut input, 20)?);

            let topic_count = take(&mut input, 1)?[0] as usize;
            let mut topics = Vec::with_capacity(topic_count);

            for _ in 0..topic_count {
                topics.push(B256::from_slice(take(&mut input, 32)?));
            }

            let data_len = take_u32(&mut input)?;
            let data = Bytes::copy_from_slice(take(&mut input, data_len)?);

            logs.push(ExecutionLog {
                address,
                topics,
                data,
            });
        }

        if !input.is_empty() {
            return Err(ReceiptDecodeError::TrailingBytes);
        }

        Ok(Self {
            status,
            gas_used,
            output,
            contract_address,
            logs,
        })
    }

    pub fn hash(&self) -> B256 {
        keccak256(self.encode_canonical())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::address;

    fn sample() -> ExecutionReceipt {
        ExecutionReceipt {
            status: ExecutionStatus::Revert,
            gas_used: 54_321,
            output: Bytes::from_static(&[1, 2, 3]),
            contract_address: Some(address!("1111111111111111111111111111111111111111")),
            logs: vec![
                ExecutionLog {
                    address: address!("2222222222222222222222222222222222222222"),
                    topics: vec![B256::repeat_byte(7), B256::repeat_byte(8)],
                    data: Bytes::from_static(&[9; 40]),
                },
                ExecutionLog {
                    address: address!("3333333333333333333333333333333333333333"),
                    topics: vec![],
                    data: Bytes::new(),
                },
            ],
        }
    }

    #[test]
    fn receipt_round_trips() {
        let receipt = sample();

        let decoded = ExecutionReceipt::decode_canonical(&receipt.encode_canonical()).unwrap();

        assert_eq!(decoded, receipt);
        assert_eq!(decoded.hash(), receipt.hash());
    }

    #[test]
    fn receipt_decode_rejects_bad_input() {
        let bytes = sample().encode_canonical();

        assert_eq!(
            ExecutionReceipt::decode_canonical(&bytes[..bytes.len() - 1]),
            Err(ReceiptDecodeError::UnexpectedEof)
        );

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            ExecutionReceipt::decode_canonical(&trailing),
            Err(ReceiptDecodeError::TrailingBytes)
        );

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert_eq!(
            ExecutionReceipt::decode_canonical(&bad_magic),
            Err(ReceiptDecodeError::InvalidMagic)
        );

        let mut bad_status = bytes;
        bad_status[4] = 9;
        assert_eq!(
            ExecutionReceipt::decode_canonical(&bad_status),
            Err(ReceiptDecodeError::InvalidStatus(9))
        );
    }
}
