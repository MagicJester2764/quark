#ifndef _STDARG_H
#define _STDARG_H
/* The compiler owns this one: variadic argument layout is part of the ABI it
   generates, not something a library can describe. */
typedef __builtin_va_list va_list;
#define va_start(v, l) __builtin_va_start(v, l)
#define va_end(v)      __builtin_va_end(v)
#define va_arg(v, t)   __builtin_va_arg(v, t)
#define va_copy(d, s)  __builtin_va_copy(d, s)
#endif
