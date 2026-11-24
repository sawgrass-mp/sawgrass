// Command livekit-load is the NET-7 load generator for LiveKit: rooms of one speaker and N-1 listeners, the
// speaker sending one Opus-sized frame every 20 ms whose payload carries its send time, and every listener
// counting what arrives and how late. It reports the same fields as voice-bench's own generator.
package main

import (
	"encoding/binary"
	"flag"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/livekit/protocol/livekit"
	lksdk "github.com/livekit/server-sdk-go/v2"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media"
)

const bucketNs = 10_000

var (
	measuring atomic.Bool
	sent      atomic.Int64
	received  atomic.Int64
	hist      [100_000]atomic.Uint32
	joined    atomic.Int64
	hdrBytes  atomic.Int64
	hdrCount  atomic.Int64
)

func record(payload []byte) {
	if !measuring.Load() || len(payload) < 9 {
		return
	}
	ts := int64(binary.LittleEndian.Uint64(payload[1:9]))
	d := time.Now().UnixNano() - ts
	i := d / bucketNs
	if i < 0 {
		i = 0
	}
	if i >= int64(len(hist)) {
		i = int64(len(hist)) - 1
	}
	hist[i].Add(1)
	received.Add(1)
}

func percentile(p float64) float64 {
	var total uint64
	counts := make([]uint64, len(hist))
	for i := range hist {
		counts[i] = uint64(hist[i].Load())
		total += counts[i]
	}
	target := uint64(p / 100 * float64(total))
	if target == 0 {
		target = 1
	}
	var seen uint64
	for i, c := range counts {
		seen += c
		if seen >= target {
			return float64(int64(i)*bucketNs+bucketNs/2) / 1e6
		}
	}
	return 0
}

// cpuSeconds is this process's user plus system time, from /proc (CLK_TCK 100).
func cpuSeconds() float64 {
	b, err := os.ReadFile("/proc/self/stat")
	if err != nil {
		return 0
	}
	s := string(b)
	f := strings.Fields(s[strings.LastIndexByte(s, ')')+1:])
	u, _ := strconv.ParseFloat(f[11], 64)
	k, _ := strconv.ParseFloat(f[12], 64)
	return (u + k) / 100
}

func connect(url, key, secret, room, identity string, cb *lksdk.RoomCallback) *lksdk.Room {
	for attempt := 0; attempt < 10; attempt++ {
		r, err := lksdk.ConnectToRoom(url, lksdk.ConnectInfo{
			APIKey: key, APISecret: secret, RoomName: room, ParticipantIdentity: identity,
		}, cb)
		if err == nil {
			return r
		}
		time.Sleep(time.Second)
	}
	fmt.Fprintf(os.Stderr, "could not join %s as %s\n", room, identity)
	return nil
}

func main() {
	url := flag.String("url", "ws://127.0.0.1:7880", "LiveKit URL")
	key := flag.String("key", "devkey", "API key")
	secret := flag.String("secret", "", "API secret")
	rooms := flag.Int("rooms", 10, "rooms")
	size := flag.Int("size", 11, "participants per room: one speaker, the rest listen")
	payload := flag.Int("payload", 59, "Opus payload bytes per frame")
	secs := flag.Int("secs", 30, "seconds to measure")
	perSecond := flag.Int("join-rate", 50, "participants joining per second")
	flag.Parse()

	var wg sync.WaitGroup
	var all []*lksdk.Room
	var mu sync.Mutex
	interval := time.Second / time.Duration(*perSecond)
	start := time.Now()
	for r := 0; r < *rooms; r++ {
		room := fmt.Sprintf("voice-%d", r)
		for p := 1; p < *size; p++ {
			wg.Add(1)
			go func(id string) {
				defer wg.Done()
				cb := &lksdk.RoomCallback{ParticipantCallback: lksdk.ParticipantCallback{
					OnTrackSubscribed: func(t *webrtc.TrackRemote, _ *lksdk.RemoteTrackPublication, _ *lksdk.RemoteParticipant) {
						go func() {
							for {
								pkt, _, err := t.ReadRTP()
								if err != nil {
									return
								}
								if measuring.Load() {
									hdrBytes.Add(int64(pkt.Header.MarshalSize()))
									hdrCount.Add(1)
								}
								record(pkt.Payload)
							}
						}()
					},
				}}
				if rm := connect(*url, *key, *secret, room, id, cb); rm != nil {
					mu.Lock()
					all = append(all, rm)
					mu.Unlock()
					joined.Add(1)
				}
			}(fmt.Sprintf("%s-listener-%d", room, p))
			time.Sleep(interval)
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			rm := connect(*url, *key, *secret, room, room+"-speaker", &lksdk.RoomCallback{})
			if rm == nil {
				return
			}
			mu.Lock()
			all = append(all, rm)
			mu.Unlock()
			joined.Add(1)
			track, err := lksdk.NewLocalSampleTrack(webrtc.RTPCodecCapability{
				MimeType: webrtc.MimeTypeOpus, ClockRate: 48000, Channels: 2,
			})
			if err != nil {
				panic(err)
			}
			if _, err := rm.LocalParticipant.PublishTrack(track, &lksdk.TrackPublicationOptions{
				Name: "voice", Source: livekit.TrackSource_MICROPHONE, DisableDTX: true,
			}); err != nil {
				panic(err)
			}
			go func() {
				buf := make([]byte, *payload)
				for i := range buf {
					buf[i] = byte(i*31 + 7)
				}
				// A valid Opus TOC byte (SILK wideband, 20 ms, one frame), then the send time.
				buf[0] = 0x48
				tick := time.NewTicker(20 * time.Millisecond)
				for range tick.C {
					binary.LittleEndian.PutUint64(buf[1:9], uint64(time.Now().UnixNano()))
					if err := track.WriteSample(media.Sample{Data: buf, Duration: 20 * time.Millisecond}, nil); err == nil && measuring.Load() {
						sent.Add(1)
					}
				}
			}()
		}()
		time.Sleep(interval)
	}
	wg.Wait()
	fmt.Printf("connected all=%d after_s=%.1f\n", joined.Load(), time.Since(start).Seconds())
	time.Sleep(10 * time.Second)
	cpu0 := cpuSeconds()
	t0 := time.Now()
	measuring.Store(true)
	fmt.Println("measuring start")
	time.Sleep(time.Duration(*secs) * time.Second)
	measuring.Store(false)
	fmt.Println("measuring stop")
	elapsed := time.Since(t0).Seconds()
	cpu := (cpuSeconds() - cpu0) / elapsed
	time.Sleep(500 * time.Millisecond)
	s := float64(sent.Load())
	expected := s * float64(*size-1)
	rx := float64(received.Load())
	fmt.Printf("result players=%d rooms=%d size=%d payload=%d frames_sent_per_s=%.0f frames_expected_per_s=%.0f frames_received_per_s=%.0f delivered=%.4f lat_p50_ms=%.2f lat_p99_ms=%.2f lat_p999_ms=%.2f gen_cpu_cores=%.3f gomaxprocs=%d rtp_header_bytes=%.1f\n",
		*rooms**size, *rooms, *size, *payload, s/elapsed, expected/elapsed, rx/elapsed, rx/max(expected, 1),
		percentile(50), percentile(99), percentile(99.9), cpu, runtime.GOMAXPROCS(0),
		float64(hdrBytes.Load())/max(float64(hdrCount.Load()), 1))
	for _, rm := range all {
		rm.Disconnect()
	}
}
