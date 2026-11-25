#pragma once
#include <stdint.h>
#include <stddef.h>
void *gp_open(const char *path);
char *gp_open_error(void);
char *gp_request(void *handle, const char *json);
void gp_close(void *handle);
void gp_string_free(char *owned);
uint8_t gp_valid_hosted_address(const uint8_t *input, size_t length);
