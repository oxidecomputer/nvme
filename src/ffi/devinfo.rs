// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::opaque_type;

opaque_type!(di_node);
pub(crate) type di_node_t = *mut di_node;
