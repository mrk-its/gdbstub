use core::fmt::{self, Debug};

use num_traits::{Num, PrimInt, Unsigned};

use crate::BeBytes;

/// Methods to read/write architecture-specific registers.
// TODO: add way to de/serialize arbitrary "missing"/"uncollected" registers.
// TODO: add (optional?) trait methods for reading/writing specific register
// (via it's GDB index)
pub trait Registers: Default {
    /// Serialize `self` into a GDB register bytestream.
    ///
    /// The registers must be serialized in the order specified by the
    /// architecture's `<target>.xml`. Missing registers are serialized by
    /// passing `None` to write_byte (which gets translated to an "xx" string
    /// within the GdbStub).
    ///
    /// e.g: for ARM:
    /// github.com/bminor/binutils-gdb/blob/master/gdb/features/arm/arm-core.xml
    fn gdb_serialize(&self, write_byte: impl FnMut(Option<u8>));

    /// Deserialize a GDB register bytestream into `self`.
    ///
    /// The bytes will be provided in the order specified by the architecture's
    /// `<target>.xml`.
    ///
    /// e.g: for ARM:
    /// github.com/bminor/binutils-gdb/blob/master/gdb/features/arm/arm-core.xml
    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()>;
}

/// Encodes architecture-specific information, such as pointer size, register
/// layout, etc...
pub trait Arch: Eq + PartialEq {
    /// The architecture's pointer size (e.g: `u32` on a 32-bit system).
    type Usize: Num + PrimInt + Unsigned + Debug + fmt::LowerHex + BeBytes;

    /// The architecture's register file
    type Registers: Registers;

    /// (optional) Return the platform's `features.xml` file.
    ///
    /// Implementing this method enables `gdb` to automatically detect the
    /// target's architecture, saving the hassle of having to run `set
    /// architecture <arch>` when starting a debugging session.
    ///
    /// These descriptions can be quite succinct. For example, the target
    /// description for an `armv4t` platform can be as simple as:
    ///
    /// ```
    /// r#"<target version="1.0"><architecture>armv4t</architecture></target>"#
    /// # ;
    /// ```
    ///
    /// See the [GDB docs](https://sourceware.org/gdb/current/onlinedocs/gdb/Target-Description-Format.html)
    /// for details on the target description XML format.
    fn target_description_xml() -> Option<&'static str> {
        None
    }
}