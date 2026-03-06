use std::mem;

use conduwuit::{
	Error, Result, arrayvec::ArrayVec, checked, debug::DebugInspect, err, utils::string,
};
use serde::{
	Deserialize, de,
	de::{DeserializeSeed, Visitor},
};

use crate::util::unhandled;

/// Deserialize into T from buffer.
#[cfg_attr(
	unabridged,
	tracing::instrument(
		name = "deserialize",
		level = "trace",
		skip_all,
		fields(len = %buf.len()),
	)
)]
pub(crate) fn from_slice<'a, T>(buf: &'a [u8]) -> Result<T>
where
	T: Deserialize<'a>,
{
	let mut deserializer = Deserializer { buf, pos: 0, rec: 0, first: true };

	T::deserialize(&mut deserializer).debug_inspect(|_| {
		deserializer
			.finished()
			.expect("deserialization failed to consume trailing bytes");
	})
}

/// Deserialization state.
pub(crate) struct Deserializer<'de> {
	buf: &'de [u8],
	pos: usize,
	rec: usize,
	first: bool,
}

/// Directive to ignore a record. This type can be used to skip deserialization
/// until the next separator is found.
#[derive(Debug, Deserialize)]
pub struct Ignore;

/// Directive to ignore all remaining records. This can be used in a sequence to
/// ignore the rest of the sequence.
#[derive(Debug, Deserialize)]
pub struct IgnoreAll;

impl<'de> Deserializer<'de> {
	const SEP: u8 = crate::ser::SEP;

	/// Determine if the input was fully consumed and error if bytes remaining.
	/// This is intended for debug assertions; not optimized for parsing logic.
	fn finished(&self) -> Result<()> {
		let pos = self.pos;
		let len = self.buf.len();
		let parsed = &self.buf[0..pos];
		let unparsed = &self.buf[pos..];
		let remain = self.remaining()?;
		let trailing_sep = remain == 1 && unparsed[0] == Self::SEP;
		(remain == 0 || trailing_sep)
			.then_some(())
			.ok_or(err!(SerdeDe(
				"{remain} trailing of {len} bytes not deserialized.\n{parsed:?}\n{unparsed:?}",
			)))
	}

	/// Called at the start of arrays and tuples
	#[inline]
	fn sequence_start(&mut self) -> bool { mem::replace(&mut self.first, true) }

	/// Called at the end of arrays and tuples
	#[inline]
	fn sequence_end(&mut self, first: bool) { self.first = first; }

	/// Consume the current record to ignore it. Inside a sequence the next
	/// record is skipped but at the top-level all records are skipped such that
	/// deserialization completes with self.finished() == Ok.
	#[inline]
	fn record_ignore(&mut self) {
		if self.pos != 0 || self.rec > 0 {
			self.record_next();
		} else {
			self.record_ignore_all();
		}
	}

	/// Consume the current and all remaining records to ignore them. Similar to
	/// Ignore at the top-level, but it can be provided in a sequence to Ignore
	/// all remaining elements.
	#[inline]
	fn record_ignore_all(&mut self) { self.record_trail(); }

	/// Consume the current record. The position pointer is moved to the start
	/// of the next record. Slice of the current record is returned.
	#[inline]
	fn record_next(&mut self) -> &'de [u8] {
		self.buf[self.pos..]
			.split(|b| *b == Deserializer::SEP)
			.inspect(|record| self.inc_pos(record.len()))
			.next()
			.expect("remainder of buf even if SEP was not found")
	}

	/// Peek at the first byte of the current record. If all records were
	/// consumed None is returned instead.
	#[inline]
	fn record_peek_byte(&self) -> Option<u8> {
		let started = self.pos != 0 || self.rec > 0;
		if started {
			debug_assert!(
				self.buf[self.pos] == Self::SEP,
				"Missing expected record separator at current position"
			);
		}

		self.buf
			.get::<usize>(self.pos + usize::from(started))
			.copied()
	}

	/// Consume the record separator such that the position cleanly points to
	/// the start of the next record. (Case for some sequences)
	#[inline]
	fn record_start(&mut self) {
		if self.pos < self.buf.len() && self.buf[self.pos] == Self::SEP {
			self.inc_pos(1);
		}

		self.inc_rec(1);
	}

	/// Consume all remaining bytes, which may include record separators,
	/// returning a raw slice.
	#[inline]
	fn record_trail(&mut self) -> &'de [u8] {
		let record = &self.buf[self.pos..];
		self.inc_pos(record.len());
		record
	}

	/// Increment the position pointer.
	#[inline]
	#[cfg_attr(
		unabridged,
		tracing::instrument(
			level = "trace",
			skip(self),
			fields(
				len = self.buf.len(),
				rem = self.remaining().unwrap_or_default().saturating_sub(n),
			),
		)
	)]
	fn inc_pos(&mut self, n: usize) {
		self.pos = self.pos.saturating_add(n);
		debug_assert!(self.pos <= self.buf.len(), "pos out of range");
	}

	#[inline]
	fn inc_rec(&mut self, n: usize) { self.rec = self.rec.saturating_add(n); }

	/// Unconsumed input bytes.
	#[inline]
	fn remaining(&self) -> Result<usize> {
		let pos = self.pos;
		let len = self.buf.len();
		checked!(len - pos)
	}
}

impl<'a, 'de: 'a> de::Deserializer<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let first = self.sequence_start();
		let val = visitor.visit_seq(&mut *self);
		self.sequence_end(first);
		val
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, visitor)))]
	fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let first = self.sequence_start();
		let val = visitor.visit_seq(&mut *self);
		self.sequence_end(first);
		val
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, visitor)))]
	fn deserialize_tuple_struct<V>(
		self,
		_name: &'static str,
		_len: usize,
		visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let first = self.sequence_start();
		let val = visitor.visit_seq(&mut *self);
		self.sequence_end(first);
		val
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let input = self.record_next();
		let mut d = serde_json::Deserializer::from_slice(input);
		d.deserialize_map(visitor).map_err(Into::into)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, visitor)))]
	fn deserialize_struct<V>(
		self,
		name: &'static str,
		fields: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let input = self.record_next();
		let mut d = serde_json::Deserializer::from_slice(input);
		d.deserialize_struct(name, fields, visitor)
			.map_err(Into::into)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, visitor)))]
	fn deserialize_unit_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		match name {
			| "Ignore" => self.record_ignore(),
			| "IgnoreAll" => self.record_ignore_all(),
			| _ => unhandled!("Unrecognized deserialization Directive {name:?}"),
		}

		visitor.visit_unit()
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, visitor)))]
	fn deserialize_newtype_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		match name {
			| "$serde_json::private::RawValue" => visitor.visit_map(self),
			| "Cbor" => visitor
				.visit_newtype_struct(&mut minicbor_serde::Deserializer::new(self.record_trail()))
				.map_err(|e| Self::Error::SerdeDe(e.to_string().into())),

			| _ => visitor.visit_newtype_struct(self),
		}
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, _visitor)))]
	fn deserialize_enum<V>(
		self,
		_name: &'static str,
		_variants: &'static [&'static str],
		_visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		unhandled!("deserialize Enum not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		if self
			.buf
			.get(self.pos)
			.is_none_or(|b| *b == Deserializer::SEP)
		{
			visitor.visit_none()
		} else {
			visitor.visit_some(self)
		}
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_bool<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize bool not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i8<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i8 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i16<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i16 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		const BYTES: usize = size_of::<i64>();

		let end = self.pos.saturating_add(BYTES);
		if end > self.buf.len() {
			return Err(Self::Error::SerdeDe("integer buffer underflow".into()));
		}
		let bytes: [u8; BYTES] = self.buf[self.pos..end].try_into().unwrap();

		self.inc_pos(BYTES);
		self.inc_rec(1);
		visitor.visit_i64(i64::from_be_bytes(bytes))
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u8<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!(
			"deserialize u8 not implemented; try dereferencing the Handle for [u8] access \
			 instead"
		)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u16<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize u16 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize u32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		const BYTES: usize = size_of::<u64>();

		let end = self.pos.saturating_add(BYTES);
		if end > self.buf.len() {
			return Err(Self::Error::SerdeDe("integer buffer underflow".into()));
		}
		let bytes: [u8; BYTES] = self.buf[self.pos..end].try_into().unwrap();

		self.inc_pos(BYTES);
		self.inc_rec(1);
		visitor.visit_u64(u64::from_be_bytes(bytes))
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_f32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize f32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_f64<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize f64 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_char<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize char not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_next();
		let out = deserialize_str(input)?;
		visitor.visit_borrowed_str(out)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_next();
		let out = string::string_from_bytes(input)?;
		visitor.visit_string(out)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_trail();
		visitor.visit_borrowed_bytes(input)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Byte Buf not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_unit<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Unit not implemented")
	}

	// this only used for $serde_json::private::RawValue at this time; see MapAccess
	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = "$serde_json::private::RawValue";
		visitor.visit_borrowed_str(input)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Ignored Any not implemented")
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip_all, fields(?self.buf))
	)]
	fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		debug_assert_eq!(
			conduwuit::debug::type_name::<V>(),
			"serde_json::value::de::<impl serde_core::de::Deserialize for \
			 serde_json::value::Value>::deserialize::ValueVisitor",
			"deserialize_any: type not expected"
		);

		match self.record_peek_byte() {
			| Some(b'{') => self.deserialize_map(visitor),
			| Some(b'[') => serde_json::Deserializer::from_slice(self.record_next())
				.deserialize_seq(visitor)
				.map_err(Into::into),

			| _ => self.deserialize_str(visitor),
		}
	}
}

impl<'a, 'de: 'a> de::SeqAccess<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
	where
		T: DeserializeSeed<'de>,
	{
		if self.pos >= self.buf.len() {
			return Ok(None);
		}

		if self.first {
			// At the first element of a sequence.
			// In our format, sequences never start with a separator unless they are empty.
			if self.buf[self.pos] == Deserializer::SEP {
				return Ok(None);
			}
		} else {
			// NOT at the first element.
			if self.buf[self.pos] == Deserializer::SEP {
				// Delimited element. Consume our separator.
				self.record_start();
			} else {
				// No separator. This was a concatenated fixed-size sequence.
				// However, if we're here, it means the caller (the Visitor) is
				// asking for ANOTHER element.
				// Since there's no separator, we might be at the end of the
				// concatenated part and looking at the next part of the outer
				// sequence.
				//
				// This is the tricky part: how do we know if the next bytes
				// belong to us (another fixed-size element) or the parent?
				//
				// In the DB format, fixed-size elements in a sequence are only
				// used when the length is known (e.g. [u64; 2] or tuple).
				// If the length is known, the Visitor will only call us that
				// many times. If it calls us MORE times, it's either an error
				// or a variable-length sequence (Vec) that is NOT delimited.
				//
				// For Vec<u64>, we expect it to be delimited if it's in a
				// larger structure, or trail if it's at the end.
				//
				// If we see NO separator, we assume we are still in the same
				// record (concatenated).
			}
		}

		self.first = false;
		seed.deserialize(&mut **self).map(Some)
	}
}

// this only used for $serde_json::private::RawValue at this time. our db
// schema doesn't have its own map format; we use json for that anyway
impl<'a, 'de: 'a> de::MapAccess<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, seed)))]
	fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
	where
		K: DeserializeSeed<'de>,
	{
		seed.deserialize(&mut **self).map(Some)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip(self, seed)))]
	fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
	where
		V: DeserializeSeed<'de>,
	{
		seed.deserialize(&mut **self)
	}
}

// activate when stable; too soon now
//#[cfg(debug_assertions)]
#[inline]
fn deserialize_str(input: &[u8]) -> Result<&str> { string::str_from_bytes(input) }

//#[cfg(not(debug_assertions))]
#[cfg(disable)]
#[inline]
fn deserialize_str(input: &[u8]) -> Result<&str> {
	// SAFETY: Strings were written by the serializer to the database. Assuming no
	// database corruption, the string will be valid. Database corruption is
	// detected via rocksdb checksums.
	unsafe { std::str::from_utf8_unchecked(input) }
}
