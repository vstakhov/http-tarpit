## HTTP Tarpit

This app represents a very slow HTTP endpoint that always timeout all requests. 
The practical purpose of it is very limited though I had to implement something similar
in the past to avoid a bad DDoS attack on Rspamd infrastructure.

I have used a simple Perl AnyEvent snippet like this:

~~~perl5
#!/usr/bin/env perl

use warnings;
use strict;
use AnyEvent;
use AnyEvent::Socket;
use AnyEvent::Handle;
use EV;

tcp_server "127.0.0.1", 8888, sub {
   my ($fh, $host, $port) = @_;
   my $hdl; $hdl = AnyEvent::Handle->new(
        fh => $fh,
        on_read => sub {
           my $w = AnyEvent->timer (
             after    => 300.0,
             cb       => sub {
               $hdl->push_write("hey");
             },
           );
        },
        on_error => sub {},
    );
}, sub {
   my ($fh, $thishost, $thisport) = @_;
   AE::log info => "Bound to $thishost, port $thisport.";
};

EV::run;

__END__
~~~

I have created this project to learn how to do it in Rust + Tokio.