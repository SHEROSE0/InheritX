#!/bin/bash

set -e

echo "==============================================="
echo "Building Comprehensive Circuit Breaker Tests"
echo "==============================================="

cd "$(dirname "$0")"

echo ""
echo "Step 1: Compiling circuit breaker tests..."
cargo test --lib circuit_breaker --no-run --release 2>&1 | head -20

echo ""
echo "Step 2: Running circuit breaker unit tests..."
cargo test --lib circuit_breaker:: --release -- --nocapture --test-threads=1

echo ""
echo "Step 3: Running external integration tests..."
cargo test --lib external_integrations::tests --release -- --nocapture --test-threads=1

echo ""
echo "==============================================="
echo "All Circuit Breaker Tests Completed!"
echo "==============================================="
