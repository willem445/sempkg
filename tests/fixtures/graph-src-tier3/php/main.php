<?php

require_once "Shapes.php";

use App\Shapes\Circle;

function summarize(float $radius): float {
    $c = new Circle($radius);
    return $c->area();
}

echo summarize(2.0);
