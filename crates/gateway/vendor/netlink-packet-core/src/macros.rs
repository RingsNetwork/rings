// SPDX-License-Identifier: MIT

#[macro_export]
macro_rules! getter {
    ($buffer: ident, $name:ident, slice, $offset:expr) => {
        impl<'a, T: AsRef<[u8]> + ?Sized> $buffer<&'a T> {
            pub fn $name(&self) -> &'a [u8] {
                &self.buffer.as_ref()[$offset]
            }
        }
    };
    ($buffer: ident, $name:ident, $ty:tt, $offset:expr) => {
        impl<'a, T: AsRef<[u8]>> $buffer<T> {
            getter!($name, $ty, $offset);
        }
    };
    ($name:ident, u8, $offset:expr) => {
        pub fn $name(&self) -> u8 {
            self.buffer.as_ref()[$offset]
        }
    };
    ($name:ident, u16, $offset:expr) => {
        pub fn $name(&self) -> u16 {
            $crate::parse_u16(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
    ($name:ident, u32, $offset:expr) => {
        pub fn $name(&self) -> u32 {
            $crate::parse_u32(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
    ($name:ident, u64, $offset:expr) => {
        pub fn $name(&self) -> u64 {
            $crate::parse_u64(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
    ($name:ident, i8, $offset:expr) => {
        pub fn $name(&self) -> i8 {
            self.buffer.as_ref()[$offset]
        }
    };
    ($name:ident, i16, $offset:expr) => {
        pub fn $name(&self) -> i16 {
            $crate::parse_i16(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
    ($name:ident, i32, $offset:expr) => {
        pub fn $name(&self) -> i32 {
            $crate::parse_i32(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
    ($name:ident, i64, $offset:expr) => {
        pub fn $name(&self) -> i64 {
            $crate::parse_i64(&self.buffer.as_ref()[$offset]).unwrap()
        }
    };
}

#[macro_export]
macro_rules! setter {
    ($buffer: ident, $name:ident, slice, $offset:expr) => {
        impl<'a, T: AsRef<[u8]> + AsMut<[u8]> + ?Sized> $buffer<&'a mut T> {
            $crate::paste! {
                pub fn [<$name _mut>](&mut self) -> &mut [u8] {
                    &mut self.buffer.as_mut()[$offset]
                }
            }
        }
    };
    ($buffer: ident, $name:ident, $ty:tt, $offset:expr) => {
        impl<'a, T: AsRef<[u8]> + AsMut<[u8]>> $buffer<T> {
            setter!($name, $ty, $offset);
        }
    };
    ($name:ident, u8, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: u8) {
                self.buffer.as_mut()[$offset] = value;
            }
        }
    };
    ($name:ident, u16, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: u16) {
                $crate::emit_u16(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
    ($name:ident, u32, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: u32) {
                $crate::emit_u32(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
    ($name:ident, u64, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: u64) {
                $crate::emit_u64(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
    ($name:ident, i8, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: i8) {
                self.buffer.as_mut()[$offset] = value;
            }
        }
    };
    ($name:ident, i16, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: i16) {
                $crate::emit_i16(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
    ($name:ident, i32, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: i32) {
                $crate::emit_i32(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
    ($name:ident, i64, $offset:expr) => {
        $crate::paste! {
            pub fn [<set_ $name>](&mut self, value: i64) {
                $crate::emit_i64(&mut self.buffer.as_mut()[$offset], value).unwrap()
            }
        }
    };
}

#[macro_export]
macro_rules! buffer {
    ($name:ident($buffer_len:expr) { $($field:ident : ($ty:tt, $offset:expr)),* $(,)? }) => {
        $crate::buffer!($name { $($field: ($ty, $offset),)* });
        $crate::buffer_check_length!($name($buffer_len));
    };

    ($name:ident { $($field:ident : ($ty:tt, $offset:expr)),* $(,)? }) => {
        $crate::buffer_common!($name);
        fields!($name {
            $($field: ($ty, $offset),)*
        });
    };

    ($name:ident, $buffer_len:expr) => {
        $crate::buffer_common!($name);
        $crate::buffer_check_length!($name($buffer_len));
    };

    ($name:ident) => {
        $crate::buffer_common!($name);
    };
}

#[macro_export]
macro_rules! fields {
    ($buffer:ident { $($name:ident : ($ty:tt, $offset:expr)),* $(,)? }) => {
        $(
            $crate::getter!($buffer, $name, $ty, $offset);
        )*

            $(
                $crate::setter!($buffer, $name, $ty, $offset);
            )*
    }
}

#[macro_export]
macro_rules! buffer_check_length {
    ($name:ident($buffer_len:expr)) => {
        impl<T: AsRef<[u8]>> $name<T> {
            pub fn new_checked(buffer: T) -> Result<Self, $crate::DecodeError> {
                let packet = Self::new(buffer);
                packet.check_buffer_length()?;
                Ok(packet)
            }

            fn check_buffer_length(&self) -> Result<(), $crate::DecodeError> {
                let len = self.buffer.as_ref().len();
                if len < $buffer_len {
                    Err($crate::DecodeError::invalid_buffer(
                        stringify!($name),
                        len,
                        $buffer_len,
                    ))
                } else {
                    Ok(())
                }
            }
        }
    };
}

#[macro_export]
macro_rules! buffer_common {
    ($name:ident) => {
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        pub struct $name<T> {
            buffer: T,
        }

        impl<T: AsRef<[u8]>> $name<T> {
            pub fn new(buffer: T) -> Self {
                Self { buffer }
            }

            pub fn into_inner(self) -> T {
                self.buffer
            }
        }

        impl<'a, T: AsRef<[u8]> + ?Sized> $name<&'a T> {
            pub fn inner(&self) -> &'a [u8] {
                &self.buffer.as_ref()[..]
            }
        }

        impl<'a, T: AsRef<[u8]> + AsMut<[u8]> + ?Sized> $name<&'a mut T> {
            pub fn inner_mut(&mut self) -> &mut [u8] {
                &mut self.buffer.as_mut()[..]
            }
        }
    };
}
