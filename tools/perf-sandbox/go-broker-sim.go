package main

import (
	"flag"
	"fmt"
	"math/rand"
	"sync"
	"sync/atomic"
	"time"
)

type semaphore struct {
	ch     chan struct{}
	active atomic.Int64
	peak   atomic.Int64
}

func newSemaphore(limit int) *semaphore {
	return &semaphore{ch: make(chan struct{}, limit)}
}

func (s *semaphore) acquire() func() {
	s.ch <- struct{}{}
	current := s.active.Add(1)
	for {
		peak := s.peak.Load()
		if current <= peak || s.peak.CompareAndSwap(peak, current) {
			break
		}
	}
	return func() {
		s.active.Add(-1)
		<-s.ch
	}
}

type uiEvent struct {
	name string
	cost time.Duration
}

type simConfig struct {
	servers          int
	handshakeLimit   int
	handshake        time.Duration
	terminalBursts   int
	terminalEvent    time.Duration
	fileEntries      int
	fileEntryCost    time.Duration
	fileIO           time.Duration
	monitorIO        time.Duration
	monitorEventCost time.Duration
}

type simResult struct {
	name           string
	elapsed        time.Duration
	handshakes     int64
	peakHandshakes int64
	uiEvents       int64
	uiBlocked      time.Duration
}

func main() {
	var cfg simConfig
	flag.IntVar(&cfg.servers, "servers", 30, "simulated server count")
	flag.IntVar(&cfg.handshakeLimit, "handshake-limit", 30, "max concurrent SSH handshakes")
	flag.DurationVar(&cfg.handshake, "handshake", 180*time.Millisecond, "simulated SSH handshake duration")
	flag.IntVar(&cfg.terminalBursts, "terminal-bursts", 8, "terminal output bursts per server")
	flag.DurationVar(&cfg.terminalEvent, "terminal-event-cost", 350*time.Microsecond, "UI cost for one terminal burst")
	flag.IntVar(&cfg.fileEntries, "file-entries", 800, "remote directory entries per server")
	flag.DurationVar(&cfg.fileEntryCost, "file-entry-cost", 42*time.Microsecond, "UI cost per file row")
	flag.DurationVar(&cfg.fileIO, "file-io", 90*time.Millisecond, "simulated remote file listing duration after handshake")
	flag.DurationVar(&cfg.monitorIO, "monitor-io", 65*time.Millisecond, "simulated remote monitor command duration after handshake")
	flag.DurationVar(&cfg.monitorEventCost, "monitor-event-cost", 3*time.Millisecond, "UI cost for one monitor update")
	flag.Parse()

	baseline := runBaseline(cfg)
	broker := runBroker(cfg)
	printResult(baseline)
	printResult(broker)
	fmt.Println()
	fmt.Printf("handshake reduction: %.1fx\n", float64(baseline.handshakes)/float64(max64(1, broker.handshakes)))
	fmt.Printf("wall-time speedup:    %.2fx\n", float64(baseline.elapsed)/float64(maxDuration(time.Millisecond, broker.elapsed)))
	fmt.Printf("UI blocked delta:     %s -> %s\n", baseline.uiBlocked.Truncate(time.Millisecond), broker.uiBlocked.Truncate(time.Millisecond))
}

func runBaseline(cfg simConfig) simResult {
	handshakeSem := newSemaphore(cfg.handshakeLimit)
	uiCh := make(chan uiEvent, cfg.servers*16)
	var handshakes atomic.Int64
	var wg sync.WaitGroup
	uiDone := consumeUI(uiCh)
	started := time.Now()

	for server := 0; server < cfg.servers; server++ {
		wg.Add(3)
		go func(server int) {
			defer wg.Done()
			simulateHandshake(handshakeSem, cfg.handshake, &handshakes, server, 0)
			for burst := 0; burst < cfg.terminalBursts; burst++ {
				uiCh <- uiEvent{name: "terminal", cost: cfg.terminalEvent}
			}
		}(server)
		go func(server int) {
			defer wg.Done()
			simulateHandshake(handshakeSem, cfg.handshake, &handshakes, server, 1)
			sleepWithJitter(cfg.fileIO, server, 11)
			uiCh <- uiEvent{name: "file-list", cost: time.Duration(cfg.fileEntries) * cfg.fileEntryCost}
		}(server)
		go func(server int) {
			defer wg.Done()
			simulateHandshake(handshakeSem, cfg.handshake, &handshakes, server, 2)
			sleepWithJitter(cfg.monitorIO, server, 17)
			uiCh <- uiEvent{name: "monitor", cost: cfg.monitorEventCost}
		}(server)
	}

	wg.Wait()
	close(uiCh)
	ui := <-uiDone
	return simResult{
		name:           "current: terminal + sftp + monitor each open SSH",
		elapsed:        time.Since(started),
		handshakes:     handshakes.Load(),
		peakHandshakes: handshakeSem.peak.Load(),
		uiEvents:       ui.events,
		uiBlocked:      ui.blocked,
	}
}

func runBroker(cfg simConfig) simResult {
	handshakeSem := newSemaphore(cfg.handshakeLimit)
	uiCh := make(chan uiEvent, cfg.servers*16)
	var handshakes atomic.Int64
	var wg sync.WaitGroup
	uiDone := consumeUI(uiCh)
	started := time.Now()

	for server := 0; server < cfg.servers; server++ {
		wg.Add(1)
		go func(server int) {
			defer wg.Done()
			simulateHandshake(handshakeSem, cfg.handshake, &handshakes, server, 0)

			var brokerWg sync.WaitGroup
			brokerWg.Add(3)
			go func() {
				defer brokerWg.Done()
				for burst := 0; burst < cfg.terminalBursts; burst++ {
					uiCh <- uiEvent{name: "terminal-batched", cost: cfg.terminalEvent / 2}
				}
			}()
			go func() {
				defer brokerWg.Done()
				time.Sleep(140 * time.Millisecond)
				sleepWithJitter(cfg.fileIO, server, 11)
				uiCh <- uiEvent{name: "file-list-virtualized", cost: time.Duration(min(cfg.fileEntries, 80)) * cfg.fileEntryCost}
			}()
			go func() {
				defer brokerWg.Done()
				time.Sleep(260 * time.Millisecond)
				sleepWithJitter(cfg.monitorIO, server, 17)
				uiCh <- uiEvent{name: "monitor-coalesced", cost: cfg.monitorEventCost / 2}
			}()
			brokerWg.Wait()
		}(server)
	}

	wg.Wait()
	close(uiCh)
	ui := <-uiDone
	return simResult{
		name:           "target: one server broker reuses SSH client + staged UI",
		elapsed:        time.Since(started),
		handshakes:     handshakes.Load(),
		peakHandshakes: handshakeSem.peak.Load(),
		uiEvents:       ui.events,
		uiBlocked:      ui.blocked,
	}
}

func simulateHandshake(sem *semaphore, base time.Duration, handshakes *atomic.Int64, server int, salt int) {
	release := sem.acquire()
	defer release()
	handshakes.Add(1)
	sleepWithJitter(base, server, salt)
}

func sleepWithJitter(base time.Duration, server int, salt int) {
	random := rand.New(rand.NewSource(int64(server*7919 + salt*104729)))
	jitter := time.Duration(random.Intn(55)) * time.Millisecond
	time.Sleep(base + jitter)
}

type uiSummary struct {
	events  int64
	blocked time.Duration
}

func consumeUI(uiCh <-chan uiEvent) <-chan uiSummary {
	done := make(chan uiSummary, 1)
	go func() {
		var summary uiSummary
		for event := range uiCh {
			summary.events++
			summary.blocked += event.cost
			time.Sleep(event.cost)
		}
		done <- summary
	}()
	return done
}

func printResult(result simResult) {
	fmt.Println()
	fmt.Println(result.name)
	fmt.Printf("  elapsed:          %s\n", result.elapsed.Truncate(time.Millisecond))
	fmt.Printf("  SSH handshakes:   %d\n", result.handshakes)
	fmt.Printf("  peak handshakes:  %d\n", result.peakHandshakes)
	fmt.Printf("  UI events:        %d\n", result.uiEvents)
	fmt.Printf("  UI blocked time:  %s\n", result.uiBlocked.Truncate(time.Millisecond))
}

func max64(a int64, b int64) int64 {
	if a > b {
		return a
	}
	return b
}

func maxDuration(a time.Duration, b time.Duration) time.Duration {
	if a > b {
		return a
	}
	return b
}

func min(a int, b int) int {
	if a < b {
		return a
	}
	return b
}
