package shapes

import "testing"

func TestRun(t *testing.T) {
	if Run() == 0 {
		t.Fatal("zero")
	}
}
