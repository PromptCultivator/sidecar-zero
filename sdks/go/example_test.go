package sidecarzero_test

import (
	"fmt"
	"time"

	sidecarzero "github.com/PromptCultivator/sidecar-zero/sdks/go"
)

func Example() {
	client, err := sidecarzero.New(
		sidecarzero.UnixPath("/nonexistent/scz.sock"),
		sidecarzero.NewDenseF32Score(256),
		sidecarzero.ThresholdFallback(17, 0.8, 0.9, 0.1),
	)
	if err != nil {
		panic(err)
	}
	defer client.Close()
	out := client.Infer(make([]float32, 256), 2*time.Millisecond)
	fmt.Println(out.Source, out.Reason, out.Value)
	// Output: fallback NOT_CONNECTED 0.1
}
