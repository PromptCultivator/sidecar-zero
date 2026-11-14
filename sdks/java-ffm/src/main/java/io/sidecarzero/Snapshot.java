package io.sidecarzero;

/**
 * What request threads see: one mapped arena and the binding to its registry, loaded with one
 * volatile read and used for the whole call (sdk-contract §4, "snap"). A rebind publishes a new
 * snapshot of the same {@link Conn}.
 */
final class Snapshot {
    final long id;
    final Conn conn;
    final Binding binding;

    Snapshot(long id, Conn conn, Binding binding) {
        this.id = id;
        this.conn = conn;
        this.binding = binding;
    }
}
