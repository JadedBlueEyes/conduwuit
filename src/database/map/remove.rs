use std::{convert::AsRef, fmt::Debug, io::Write};

use arrayvec::ArrayVec;
use conduit::implement;
use serde::Serialize;

use crate::{ser, util::or_else};

#[implement(super::Map)]
pub fn del<K>(&self, key: &K)
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = Vec::<u8>::with_capacity(64);
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
pub fn adel<const MAX: usize, K>(&self, key: &K)
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), fields(%self), level = "trace")]
pub fn bdel<K, B>(&self, key: &K, buf: &mut B)
where
	K: Serialize + ?Sized + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize deletion key");
	self.remove(key);
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, key), fields(%self), level = "trace")]
pub fn remove<K>(&self, key: &K)
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	let write_options = &self.write_options;
	self.db
		.db
		.delete_cf_opt(&self.cf(), key, write_options)
		.or_else(or_else)
		.expect("database remove error");

	if !self.db.corked() {
		self.db.flush().expect("database flush error");
	}
}
