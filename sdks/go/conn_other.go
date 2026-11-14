//go:build !linux

package sidecarzero

// SDKVersion is what HELLO announces: major << 16 | minor << 8 | patch.
const SDKVersion = 0x00_01_00

// background off Linux: there is no shared memory to map, so the client never connects and
// every call falls back with NOT_CONNECTED (sdk-contract §9).
func (k *core) background() {
	defer close(k.done)
	k.setState(Disconnected)
	<-k.stopCh
}
