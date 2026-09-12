define if_member
@if cargo metadata --no-deps --format-version 1 2>/dev/null | grep -q '"name":"$(1)"'; then \
		echo "+ $(2)"; $(2); \
	else \
		echo "- skipped: $(1) is not in this workspace (proprietary; absent from the public export)"; \
	fi
endef

probe:
	
