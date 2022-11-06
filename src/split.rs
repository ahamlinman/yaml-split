use std::ffi::{c_void, CStr};
use std::io::Read;
use std::mem::MaybeUninit;

use unsafe_libyaml::*;

pub struct Splitter<R>
where
    R: Read,
{
    parser: *mut yaml_parser_t,
    reader: *mut R,
}

impl<R> Splitter<R>
where
    R: Read,
{
    pub fn new(reader: R) -> Self {
        let parser = unsafe {
            let mut parser_uninit = Box::new(MaybeUninit::<yaml_parser_t>::uninit());
            if yaml_parser_initialize(parser_uninit.as_mut_ptr()).fail {
                panic!("failed to initialize YAML parser");
            }
            Box::into_raw(parser_uninit) as *mut yaml_parser_t
        };

        let reader = Box::into_raw(Box::new(reader));
        unsafe {
            yaml_parser_set_input(parser, Self::read_callback, reader as *mut c_void);
            yaml_parser_set_encoding(parser, YAML_UTF8_ENCODING);
        }

        Self { parser, reader }
    }

    fn read_callback(reader: *mut c_void, buffer: *mut u8, size: u64, size_read: *mut u64) -> i32 {
        // SAFETY: Once we take ownership of the reader during construction,
        // this is the only function that ever constructs a &mut to it before it
        // is dropped.
        let reader = reader as *mut R;

        // SAFETY: We assume that libyaml gives us a valid buffer.
        let buf = unsafe { std::slice::from_raw_parts_mut(buffer, size as usize) };

        let len = match unsafe { (*reader).read(buf) } {
            Ok(len) => len,
            Err(_) => return 0,
        };

        // SAFETY: We assume that libyaml gives us a valid place to write this.
        unsafe { *size_read = len as u64 };
        1
    }
}

impl<R> Iterator for Splitter<R>
where
    R: Read,
{
    type Item = (u32, u64);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut event = unsafe {
                let mut event: MaybeUninit<yaml_event_t> = MaybeUninit::uninit();
                let result = yaml_parser_parse(self.parser, event.as_mut_ptr());
                if result.fail {
                    let problem_str = CStr::from_ptr((*self.parser).problem).to_str().unwrap();
                    panic!(
                        "something bad happened ({}): {} @ {}",
                        (*self.parser).error as u32,
                        problem_str,
                        (*self.parser).problem_offset,
                    );
                }
                event.assume_init()
            };

            let result = match event.type_ {
                YAML_STREAM_END_EVENT => None,
                YAML_DOCUMENT_START_EVENT => Some((event.type_ as u32, event.start_mark.index)),
                YAML_DOCUMENT_END_EVENT => Some((event.type_ as u32, event.end_mark.index)),
                _ => {
                    unsafe { yaml_event_delete(&mut event) };
                    continue;
                }
            };
            unsafe { yaml_event_delete(&mut event) };
            return result;
        }
    }
}

impl<R> Drop for Splitter<R>
where
    R: Read,
{
    fn drop(&mut self) {
        unsafe {
            yaml_parser_delete(self.parser);
            let _ = Box::from_raw(self.parser);
            let _ = Box::from_raw(self.reader);
        }
    }
}
