// use std::marker::PhantomData;
//
// use crate::readers::Reader;
//
// #[allow(dead_code)]
// pub struct Operation<R, T>
// where
//     R: Reader<T>,
// {
//     reader: R,
//     _marker: PhantomData<T>,
// }
//
//
// impl<R, T> Operation<R, T>
// where
//     R: Reader<T>,
// {
//     pub fn new(reader: R) -> Self {
//         Self {
//             reader,
//             _marker: PhantomData,
//         }
//     }
// }
