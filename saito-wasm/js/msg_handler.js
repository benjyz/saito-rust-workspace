export class MsgHandler {
    static send_message(peer_index, buffer) {
        return global.shared_methods.send_message(peer_index, buffer);
    }

    static send_message_to_all(buffer, exceptions) {
        return global.shared_methods.send_message_to_all(buffer, exceptions);
    }

    static connect_to_peer(peer_data) {
        return global.shared_methods.connect_to_peer(peer_data);
    }

    static write_value(key, value) {
        return global.shared_methods.write_value(key, value);
    }

    static read_value(key) {
        return global.shared_methods.read_value(key);
    }

    static load_block_file_list() {
        return global.shared_methods.load_block_file_list();
    }

    static is_existing_file(key) {
        return global.shared_methods.is_existing_file(key);
    }

    static remove_value(key) {
        return global.shared_methods.remove_value(key);
    }

    static disconnect_from_peer(peer_index) {
        return global.shared_methods.disconnect_from_peer(peer_index);
    }

    static fetch_block_from_peer(hash, peer_index, url) {
        return global.shared_methods.fetch_block_from_peer(hash, peer_index, url);
    }
}