# Makefile for edgefirst-camera - Software Process Automation
#
# This Makefile implements workflows from the Au-Zone Software Process Specification:
#   - Code formatting standards
#   - SBOM generation and license policy validation
#   - Pre-release quality checks and version verification
#
# Synchronized with SPS version: 2.0 (2025-11-24)

# ===========================================================================
# PROJECT CONFIGURATION
# ===========================================================================

PROJECT_TYPE := rust
SRC_DIRS := src g2d-sys/src tests benches
TEST_CMD := cargo test --workspace
VERSION_FILE := Cargo.toml
VERSION_FILES := Cargo.toml g2d-sys/Cargo.toml CHANGELOG.md

# ===========================================================================
# STANDARD TARGETS
# ===========================================================================

.PHONY: help
help:
	@echo "Available targets:"
	@echo "  make format         - Format Rust code with cargo fmt"
	@echo "  make lint           - Run cargo clippy"
	@echo "  make test           - Run test suite"
	@echo "  make sbom           - Generate SBOM and check license policy"
	@echo "  make verify-version - Verify version consistency across files"
	@echo "  make pre-release    - Run all pre-release checks"
	@echo "  make clean          - Clean build artifacts"
	@echo ""
	@echo "Configuration:"
	@echo "  PROJECT_TYPE  = $(PROJECT_TYPE)"
	@echo "  SRC_DIRS      = $(SRC_DIRS)"
	@echo "  VERSION_FILE  = $(VERSION_FILE)"

# Format source code
.PHONY: format
format:
	@echo "Formatting Rust code..."
	@cargo +nightly fmt --all || cargo fmt --all
	@echo "✓ Formatting complete"

# Lint code
.PHONY: lint
lint:
	@echo "Running clippy..."
	@cargo clippy --workspace --all-features -- -D warnings
	@echo "✓ Linting complete"

# Run tests
.PHONY: test
test:
	@echo "Running tests..."
	@$(TEST_CMD)
	@echo "✓ Tests complete"

# Generate SBOM and validate license policy
.PHONY: sbom
sbom:
	@echo "Generating SBOM..."
	@bash .github/scripts/generate_sbom.sh
	@echo "✓ SBOM generation complete"

# Verify version consistency
.PHONY: verify-version
verify-version:
	@echo "Verifying version consistency..."
	@echo "Extracting version from $(VERSION_FILE)..."
	@VERSION=$$(grep '^version' $(VERSION_FILE) | head -1 | sed 's/.*"\(.*\)".*/\1/'); \
	echo "Version found: $$VERSION"; \
	for file in $(VERSION_FILES); do \
		if [ -f "$$file" ]; then \
			if ! grep -q "$$VERSION" "$$file"; then \
				echo "✗ ERROR: Version $$VERSION not found in $$file"; \
				exit 1; \
			else \
				echo "  ✓ $$file contains $$VERSION"; \
			fi; \
		fi; \
	done
	@echo "✓ Version verification complete"

# Pre-release checklist
.PHONY: pre-release
pre-release: format lint test sbom verify-version
	@echo ""
	@echo "=================================================="
	@echo "Pre-Release Checklist Complete"
	@echo "=================================================="
	@echo "✓ Code formatting verified"
	@echo "✓ Linting passed"
	@echo "✓ Tests passed"
	@echo "✓ SBOM generated and validated"
	@echo "✓ Version consistency verified"
	@echo ""
	@VERSION=$$(grep '^version' $(VERSION_FILE) | head -1 | sed 's/.*"\(.*\)".*/\1/'); \
	echo "Next steps:"; \
	echo "  1. Review changes: git status && git diff"; \
	echo "  2. Commit: git add -A && git commit -m 'PROJECTKEY-###: Prepare version $$VERSION'"; \
	echo "  3. Tag: git tag -a v$$VERSION -m 'Release v$$VERSION'"; \
	echo "  4. Push: git push origin main --tags"; \
	echo "  5. Wait for CI/CD to pass"

# Clean build artifacts
.PHONY: clean
clean:
	@echo "Cleaning build artifacts..."
	@cargo clean
	@rm -f source-sbom*.json sbom.json deps-sbom.json
	@echo "✓ Clean complete"

# Additional Rust-specific targets
.PHONY: build
build:
	@echo "Building project..."
	@cargo build --release
	@echo "✓ Build complete"

.PHONY: doc
doc:
	@echo "Generating documentation..."
	@cargo doc --no-deps --open
	@echo "✓ Documentation complete"

.PHONY: bench
bench:
	@echo "Running benchmarks..."
	@cargo bench
	@echo "✓ Benchmarks complete"

.PHONY: coverage
coverage:
	@echo "Generating coverage report..."
	@cargo llvm-cov --all-features --workspace --html
	@echo "✓ Coverage report generated: target/llvm-cov/html/index.html"
