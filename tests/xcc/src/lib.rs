#![cfg(test)]

#[test]
fn test_import() {
    idl_gen::test_oasis_interface("a", "ServiceA");
}
