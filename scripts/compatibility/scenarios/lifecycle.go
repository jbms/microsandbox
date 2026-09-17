// Common released lifecycle API; deliberately independent of dedicated restore APIs.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	msb "github.com/superradcompany/microsandbox/sdk/go"
	"os"
	"os/exec"
	"time"
)

func exercise() ([]string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 180*time.Second)
	defer cancel()
	existing := os.Getenv("MSB_COMPAT_EXISTING")
	counts := []int{0, 1, 3}
	if existing != "" {
		counts = []int{1}
	}
	passed := []string{}
	for _, count := range counts {
		name := existing
		if name == "" {
			name = fmt.Sprintf("compat-common-%d", count)
		}
		mounts := map[string]msb.MountConfig{}
		for i := 0; i < count; i++ {
			mounts[fmt.Sprintf("/compat-data-%d", i)] = msb.Mount.Tmpfs(msb.TmpfsOptions{SizeMiB: 8})
		}
		var sandbox *msb.Sandbox
		var err error
		if existing != "" {
			sandbox, err = msb.StartSandbox(ctx, name)
		} else {
			sandbox, err = msb.CreateSandbox(ctx, name, msb.WithImage(os.Getenv("MSB_COMPAT_IMAGE")),
				msb.WithMemory(256), msb.WithCPUs(1), msb.WithMounts(mounts), msb.WithEnv(map[string]string{"COMPAT_MARKER": "retained"}))
		}
		if err != nil {
			return passed, err
		}
		for restart := 0; restart < 2; restart++ {
			if output, err := exec.Command(os.Getenv("MSB_COMPAT_PYTHON"), os.Getenv("MSB_COMPAT_VERIFY_RUNTIME"), name).CombinedOutput(); err != nil {
				return passed, fmt.Errorf("identity: %s: %w", output, err)
			}
			script := `test "$COMPAT_MARKER" = retained; `
			if existing == "" && restart == 0 {
				script += "printf retained > /root/compat-marker; "
			}
			script += `test "$(cat /root/compat-marker)" = retained; `
			for i := 0; i < count; i++ {
				script += fmt.Sprintf(`test "$(stat -f -c %%T /compat-data-%d)" = tmpfs; `, i)
			}
			output, err := sandbox.Exec(ctx, "sh", []string{"-ec", script})
			if err != nil {
				return passed, err
			}
			if !output.Success() {
				return passed, fmt.Errorf("guest assertion: %s", output.Stderr())
			}
			if err := sandbox.Stop(ctx); err != nil {
				return passed, err
			}
			if err := sandbox.Close(); err != nil {
				return passed, err
			}
			if restart == 0 {
				sandbox, err = msb.StartSandbox(ctx, name)
				if err != nil {
					return passed, err
				}
			}
		}
		passed = append(passed, name+"/runtime-env-mounts-disk-restart")
		if existing == "" {
			if err := msb.RemoveSandbox(ctx, name); err != nil {
				return passed, err
			}
		}
	}
	return passed, nil
}

func main() {
	passed, err := exercise()
	report := map[string]any{"status": "passed", "passed": passed}
	if err != nil {
		report["status"] = "failed"
		report["error"] = err.Error()
	}
	data, _ := json.Marshal(report)
	if writeErr := os.WriteFile(os.Getenv("MSB_COMPAT_REPORT"), data, 0600); writeErr != nil {
		panic(writeErr)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
