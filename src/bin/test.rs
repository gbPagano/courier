use std::any::type_name;

fn get_struct_name<T>() -> &'static str {
    type_name::<T>()
}

// Uso:
struct MyStruct;

fn main() {
    println!("{}", get_struct_name::<MyStruct>()); // "my_crate::MyStruct"

    // Ou diretamente:
    println!("{}", type_name::<MyStruct>());
}
