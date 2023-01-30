// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Definitions for test selection codes used by the "multi_test" test.
extern crate alloc;

use alloc::{string::String, vec::Vec};

use risc0_zeroio::{Deserialize as ZeroioDeserialize, Serialize as ZeroioSerialize};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, ZeroioSerialize, ZeroioDeserialize)]
pub struct TestStruct {
    pub foo: String,
    pub bar: u32,
}

#[derive(ZeroioSerialize, ZeroioDeserialize, Debug)]
pub enum MultiTestSpec {
    DoNothing,
    ShaConforms,
    ShaCycleCount,
    ShaDigest {
        data: Vec<u8>,
    },
    ShaSerializeDigest {
        data: TestStruct,
    },
    EventTrace,
    Profiler,
    Fail,
    ReadWriteMem {
        /// Tuples of (address, value). Zero means read the value and
        /// output it; nonzero means write that value.
        values: Vec<(u32, u32)>,
    },
    SendRecv {
        channel_id: u32,
        count: u32,
    },
}
