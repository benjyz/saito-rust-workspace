#[cfg(test)]
mod tests {
    use crate::saito::rust_io_handler::RustIOHandler;
    use crate::test::test_io_handler::TestIOHandler;
    use crate::test::test_manager::TestManager;
    use log::info;
    use saito_core::common::interface_io::InterfaceIO;
    use saito_core::core::data::wallet::Wallet;

    #[tokio::test]
    #[serial_test::serial]
    async fn save_and_restore_wallet_test() {
        info!("current dir = {:?}", std::env::current_dir().unwrap());
        TestManager::clear_data_folder().await;
        let mut wallet = Wallet::new();
        let publickey1 = wallet.get_publickey().clone();
        let privatekey1 = wallet.get_privatekey().clone();

        let mut io_handler: Box<dyn InterfaceIO + Send + Sync> = Box::new(TestIOHandler::new());

        wallet.save(&mut io_handler).await;

        wallet = Wallet::new();

        assert_ne!(wallet.get_publickey(), publickey1);
        assert_ne!(wallet.get_privatekey(), privatekey1);

        wallet.load(&mut io_handler).await;

        assert_eq!(wallet.get_publickey(), publickey1);
        assert_eq!(wallet.get_privatekey(), privatekey1);
    }
}
